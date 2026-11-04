//! Conformance tests for MessageService, AuthorService and the Redis cache behaviour, driven
//! through a real tonic client against the in-process server, Postgres and Redis.

mod common;

use common::{CacheMode, TestEnv, assert_status, violations};
use message_service::{
    cache::{entry_key, lock_key},
    pb::{
        CreateAuthorRequest, CreateMessageRequest, DeleteAuthorRequest, DeleteMessageRequest,
        GetAuthorRequest, GetMessageRequest, ListAuthorsRequest, ListMessagesRequest, Message,
        UpdateAuthorRequest, UpdateMessageRequest,
    },
};
use prost::Message as _;
use std::time::{Duration, Instant};
use tonic::Code;
use uuid::Uuid;

macro_rules! env {
    ($mode:expr) => {
        match TestEnv::start($mode).await {
            Some(t) => t,
            None => return,
        }
    };
}

fn random_uuid() -> String {
    Uuid::new_v4().to_string()
}

// ---------------------------------------------------------------- MessageService

#[tokio::test]
async fn create_message_returns_version_zero_trimmed_with_author() {
    let mut t = env!(CacheMode::Redis);
    let a = t.author("Ada").await;
    let m = t
        .messages
        .create_message(CreateMessageRequest {
            title: "  Hello  ".into(),
            content: "\tworld\n".into(),
            author_id: a.id.clone(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (m.title.as_str(), m.content.as_str(), m.version),
        ("Hello", "world", 0)
    );
    assert_eq!(m.author.as_ref().unwrap().id, a.id);
    assert!(m.created_at.is_some());
    assert!(Uuid::parse_str(&m.id).is_ok());
}

#[tokio::test]
async fn create_message_collects_every_violation() {
    let mut t = env!(CacheMode::Disabled);
    let st = assert_status(
        t.messages
            .create_message(CreateMessageRequest {
                title: "   ".into(),
                content: "x".repeat(1001),
                author_id: "nope".into(),
            })
            .await,
        Code::InvalidArgument,
        "BAD_USER_INPUT",
    );
    assert_eq!(
        violations(&st),
        vec![
            (
                "title".into(),
                "title is required and cannot be blank".into()
            ),
            (
                "content".into(),
                "content cannot exceed 1000 characters".into()
            ),
            ("author_id".into(), "author_id must be a valid UUID".into()),
        ]
    );
}

#[tokio::test]
async fn create_message_length_limits_are_inclusive_and_utf16() {
    let mut t = env!(CacheMode::Disabled);
    let a = t.author("Len").await;
    // 100 chars OK; 101 rejected.
    t.message(&a, &"t".repeat(100), "c").await;
    let r = t
        .messages
        .create_message(CreateMessageRequest {
            title: "t".repeat(101),
            content: "c".into(),
            author_id: a.id.clone(),
        })
        .await;
    assert_status(r, Code::InvalidArgument, "BAD_USER_INPUT");
    // 50 emoji = 100 UTF-16 units: fits the 100 limit; 51 do not (Java String.length()).
    t.message(&a, &"😀".repeat(50), "c").await;
    let r = t
        .messages
        .create_message(CreateMessageRequest {
            title: "😀".repeat(51),
            content: "c".into(),
            author_id: a.id.clone(),
        })
        .await;
    assert_status(r, Code::InvalidArgument, "BAD_USER_INPUT");
    // NUL is rejected with its own message.
    let st = assert_status(
        t.messages
            .create_message(CreateMessageRequest {
                title: "a\0b".into(),
                content: "c".into(),
                author_id: a.id.clone(),
            })
            .await,
        Code::InvalidArgument,
        "BAD_USER_INPUT",
    );
    assert_eq!(violations(&st)[0].1, "title cannot contain NUL characters");
}

#[tokio::test]
async fn create_message_unknown_author_is_not_found() {
    let mut t = env!(CacheMode::Disabled);
    let id = random_uuid();
    let st = assert_status(
        t.messages
            .create_message(CreateMessageRequest {
                title: "t".into(),
                content: "c".into(),
                author_id: id.clone(),
            })
            .await,
        Code::NotFound,
        "NOT_FOUND",
    );
    assert_eq!(
        st.message(),
        format!("Author with ID '{id}' was not found.")
    );
}

#[tokio::test]
async fn get_message_roundtrip_and_errors() {
    let mut t = env!(CacheMode::Redis);
    let a = t.author("Grace").await;
    let m = t.message(&a, "T", "C").await;
    let got = t
        .messages
        .get_message(GetMessageRequest { id: m.id.clone() })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got, m);

    let missing = random_uuid();
    let st = assert_status(
        t.messages
            .get_message(GetMessageRequest {
                id: missing.clone(),
            })
            .await,
        Code::NotFound,
        "NOT_FOUND",
    );
    assert_eq!(
        st.message(),
        format!("Message with ID '{missing}' was not found.")
    );

    let st = assert_status(
        t.messages
            .get_message(GetMessageRequest { id: "123".into() })
            .await,
        Code::InvalidArgument,
        "BAD_USER_INPUT",
    );
    assert_eq!(
        violations(&st),
        vec![("id".into(), "id must be a valid UUID".into())]
    );
    // non-canonical forms (no hyphens / braces / upper-case-with-urn) are rejected as in Java.
    for bad in [
        Uuid::new_v4().simple().to_string(),
        format!("{{{}}}", Uuid::new_v4()),
    ] {
        assert_status(
            t.messages.get_message(GetMessageRequest { id: bad }).await,
            Code::InvalidArgument,
            "BAD_USER_INPUT",
        );
    }
}

#[tokio::test]
async fn list_messages_newest_first_with_total_and_paging() {
    let mut t = env!(CacheMode::Disabled);
    let a = t.author("Lister").await;
    let mut ids = vec![];
    for i in 0..5 {
        ids.push(t.message(&a, &format!("m{i}"), "c").await.id);
    }
    let all = t
        .messages
        .list_messages(ListMessagesRequest::default())
        .await
        .unwrap()
        .into_inner();
    assert_eq!(all.total_count, 5);
    let got: Vec<_> = all.items.iter().map(|m| m.id.clone()).collect();
    ids.reverse(); // newest first
    assert_eq!(got, ids);
    assert!(
        all.items
            .iter()
            .all(|m| m.author.as_ref().unwrap().id == a.id)
    );

    let page = t
        .messages
        .list_messages(ListMessagesRequest {
            limit: Some(2),
            offset: Some(1),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(page.total_count, 5);
    assert_eq!(
        page.items.iter().map(|m| m.id.clone()).collect::<Vec<_>>(),
        ids[1..3]
    );

    let beyond = t
        .messages
        .list_messages(ListMessagesRequest {
            limit: Some(10),
            offset: Some(50),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(beyond.items.is_empty());
    assert_eq!(beyond.total_count, 5);
}

#[tokio::test]
async fn list_messages_pagination_validation() {
    let mut t = env!(CacheMode::Disabled);
    for (limit, offset) in [
        (Some(0), None),
        (Some(201), None),
        (None, Some(-1)),
        (Some(-5), Some(-5)),
    ] {
        let r = t
            .messages
            .list_messages(ListMessagesRequest { limit, offset })
            .await;
        let st = assert_status(r, Code::InvalidArgument, "BAD_USER_INPUT");
        assert!(!violations(&st).is_empty());
    }
    let st = t
        .messages
        .list_messages(ListMessagesRequest {
            limit: Some(0),
            offset: Some(-1),
        })
        .await
        .unwrap_err();
    let fields: Vec<_> = violations(&st).into_iter().map(|v| v.0).collect();
    assert_eq!(fields, ["limit", "offset"]);
    // 200 is the inclusive maximum.
    t.messages
        .list_messages(ListMessagesRequest {
            limit: Some(200),
            offset: Some(0),
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn keyset_pagination_is_stable_with_equal_timestamps() {
    let mut t = env!(CacheMode::Disabled);
    let a = t.author("Ties").await;
    for i in 0..6 {
        t.message(&a, &format!("m{i}"), "c").await;
    }
    // Force identical created_at so the (created_at, id) tiebreak decides the order.
    sqlx::query("UPDATE messages SET created_at = '2026-01-01T00:00:00Z'")
        .execute(&t.pool)
        .await
        .unwrap();
    let mut seen = vec![];
    for offset in [0, 2, 4] {
        let p = t
            .messages
            .list_messages(ListMessagesRequest {
                limit: Some(2),
                offset: Some(offset),
            })
            .await
            .unwrap()
            .into_inner();
        seen.extend(p.items.into_iter().map(|m| m.id));
    }
    let mut dedup = seen.clone();
    dedup.sort();
    dedup.dedup();
    assert_eq!(seen.len(), 6);
    assert_eq!(dedup.len(), 6, "pages overlap or skip rows");
}

#[tokio::test]
async fn update_message_bumps_version_and_keeps_title_when_absent() {
    let mut t = env!(CacheMode::Redis);
    let a = t.author("Upd").await;
    let m = t.message(&a, "old", "old content").await;
    let u = t
        .messages
        .update_message(UpdateMessageRequest {
            id: m.id.clone(),
            title: None,
            content: " new content ".into(),
            version: Some(0),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (u.title.as_str(), u.content.as_str(), u.version),
        ("old", "new content", 1)
    );
    assert_eq!(u.created_at, m.created_at);
    assert_eq!(u.author.as_ref().unwrap().id, a.id);

    let u2 = t
        .messages
        .update_message(UpdateMessageRequest {
            id: m.id.clone(),
            title: Some("renamed".into()),
            content: "c".into(),
            version: Some(1),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!((u2.title.as_str(), u2.version), ("renamed", 2));
}

#[tokio::test]
async fn update_message_stale_version_is_aborted_conflict() {
    let mut t = env!(CacheMode::Redis);
    let a = t.author("Stale").await;
    let m = t.message(&a, "t", "c").await;
    t.messages
        .update_message(UpdateMessageRequest {
            id: m.id.clone(),
            title: None,
            content: "c1".into(),
            version: Some(0),
        })
        .await
        .unwrap();
    let st = assert_status(
        t.messages
            .update_message(UpdateMessageRequest {
                id: m.id.clone(),
                title: None,
                content: "c2".into(),
                version: Some(0),
            })
            .await,
        Code::Aborted,
        "CONFLICT",
    );
    assert!(
        st.message()
            .starts_with(&format!("Message with ID '{}'", m.id)),
        "{}",
        st.message()
    );
    // the loser changed nothing
    let cur = t
        .messages
        .get_message(GetMessageRequest { id: m.id })
        .await
        .unwrap()
        .into_inner();
    assert_eq!((cur.content.as_str(), cur.version), ("c1", 1));
}

#[tokio::test]
async fn update_message_not_found_and_validation() {
    let mut t = env!(CacheMode::Disabled);
    assert_status(
        t.messages
            .update_message(UpdateMessageRequest {
                id: random_uuid(),
                title: None,
                content: "c".into(),
                version: Some(0),
            })
            .await,
        Code::NotFound,
        "NOT_FOUND",
    );
    let st = t
        .messages
        .update_message(UpdateMessageRequest {
            id: "x".into(),
            title: Some(" ".into()),
            content: "".into(),
            version: None,
        })
        .await
        .unwrap_err();
    assert_eq!(
        violations(&st),
        vec![
            ("id".into(), "id must be a valid UUID".into()),
            (
                "title".into(),
                "title is required and cannot be blank".into()
            ),
            (
                "content".into(),
                "content is required and cannot be blank".into()
            ),
            ("version".into(), "version is required".into()),
        ]
    );
    let st = t
        .messages
        .update_message(UpdateMessageRequest {
            id: random_uuid(),
            title: None,
            content: "c".into(),
            version: Some(-1),
        })
        .await
        .unwrap_err();
    assert_eq!(
        violations(&st),
        vec![("version".into(), "version must be 0 or greater".into())]
    );
}

#[tokio::test]
async fn delete_message_then_gone() {
    let mut t = env!(CacheMode::Redis);
    let a = t.author("Del").await;
    let m = t.message(&a, "t", "c").await;
    t.messages
        .delete_message(DeleteMessageRequest { id: m.id.clone() })
        .await
        .unwrap();
    assert_status(
        t.messages
            .get_message(GetMessageRequest { id: m.id.clone() })
            .await,
        Code::NotFound,
        "NOT_FOUND",
    );
    assert_status(
        t.messages
            .delete_message(DeleteMessageRequest { id: m.id })
            .await,
        Code::NotFound,
        "NOT_FOUND",
    );
    assert_status(
        t.messages
            .delete_message(DeleteMessageRequest { id: "zz".into() })
            .await,
        Code::InvalidArgument,
        "BAD_USER_INPUT",
    );
}

/// Optimistic locking under real contention: of N concurrent writers holding the same version
/// exactly one wins, the others get ABORTED/CONFLICT, and the row ends at version + 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_updates_with_same_version_have_exactly_one_winner() {
    let mut t = env!(CacheMode::Redis);
    let a = t.author("Race").await;
    let m = t.message(&a, "t", "c").await;
    let mut tasks = vec![];
    for i in 0..16 {
        let mut c = t.messages.clone();
        let id = m.id.clone();
        tasks.push(tokio::spawn(async move {
            c.update_message(UpdateMessageRequest {
                id,
                title: None,
                content: format!("writer {i}"),
                version: Some(0),
            })
            .await
        }));
    }
    let mut ok = 0;
    let mut aborted = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(_) => ok += 1,
            Err(st) => {
                assert_eq!(st.code(), Code::Aborted, "{st:?}");
                aborted += 1;
            }
        }
    }
    assert_eq!((ok, aborted), (1, 15));
    let cur = t
        .messages
        .get_message(GetMessageRequest { id: m.id })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(cur.version, 1);
}

#[tokio::test]
async fn oversized_request_is_rejected_by_the_transport() {
    let mut t = env!(CacheMode::Disabled);
    let r = t
        .messages
        .create_message(CreateMessageRequest {
            title: "t".into(),
            content: "x".repeat(20 * 1024),
            author_id: random_uuid(),
        })
        .await;
    // tonic reports an over-limit request as OUT_OF_RANGE (grpc-java: RESOURCE_EXHAUSTED);
    // either way it is rejected before the handler runs.
    assert_eq!(r.unwrap_err().code(), Code::OutOfRange);
}

// ---------------------------------------------------------------- AuthorService

#[tokio::test]
async fn create_author_validates_and_enforces_unique_email() {
    let mut t = env!(CacheMode::Disabled);
    let a = t
        .authors
        .create_author(CreateAuthorRequest {
            name: " Linus ".into(),
            email: "  linus@kernel.org ".into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (a.name.as_str(), a.email.as_str()),
        ("Linus", "linus@kernel.org")
    );
    assert!(a.created_at.is_some());

    let st = assert_status(
        t.authors
            .create_author(CreateAuthorRequest {
                name: "Other".into(),
                email: "linus@kernel.org".into(),
            })
            .await,
        Code::AlreadyExists,
        "CONFLICT",
    );
    assert!(
        st.message().contains("linus@kernel.org"),
        "{}",
        st.message()
    );

    let st = t
        .authors
        .create_author(CreateAuthorRequest {
            name: "n".repeat(51),
            email: "not-an-email".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(
        violations(&st),
        vec![
            ("name".into(), "name cannot exceed 50 characters".into()),
            ("email".into(), "email must be a valid email address".into()),
        ]
    );
    for bad in ["a@b", "@b.c", "a@.c", "a@b.", "a b@c.d", "a@@b.c"] {
        assert_status(
            t.authors
                .create_author(CreateAuthorRequest {
                    name: "n".into(),
                    email: bad.into(),
                })
                .await,
            Code::InvalidArgument,
            "BAD_USER_INPUT",
        );
    }
}

#[tokio::test]
async fn list_authors_oldest_first_total_and_validation() {
    let mut t = env!(CacheMode::Disabled);
    let mut ids = vec![];
    for i in 0..4 {
        ids.push(t.author(&format!("a{i}")).await.id);
    }
    let all = t
        .authors
        .list_authors(ListAuthorsRequest::default())
        .await
        .unwrap()
        .into_inner();
    assert_eq!(all.total_count, 4);
    assert_eq!(
        all.items.iter().map(|a| a.id.clone()).collect::<Vec<_>>(),
        ids
    );
    let page = t
        .authors
        .list_authors(ListAuthorsRequest {
            limit: Some(2),
            offset: Some(2),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        page.items.iter().map(|a| a.id.clone()).collect::<Vec<_>>(),
        ids[2..]
    );
    assert_status(
        t.authors
            .list_authors(ListAuthorsRequest {
                limit: Some(0),
                offset: None,
            })
            .await,
        Code::InvalidArgument,
        "BAD_USER_INPUT",
    );
}

#[tokio::test]
async fn get_author_with_and_without_messages_and_paging() {
    let mut t = env!(CacheMode::Disabled);
    let a = t.author("Writer").await;
    let other = t.author("Other").await;
    let mut ids = vec![];
    for i in 0..5 {
        ids.push(t.message(&a, &format!("m{i}"), "c").await.id);
    }
    t.message(&other, "not mine", "c").await;
    ids.reverse(); // newest first

    let plain = t
        .authors
        .get_author(GetAuthorRequest {
            id: a.id.clone(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(plain.author.as_ref().unwrap(), &a);
    assert!(plain.messages.is_empty());
    assert_eq!(plain.next_messages_offset, None);

    // walk all pages of 2
    let mut seen = vec![];
    let mut offset = Some(0i64);
    let mut pages = 0;
    while let Some(o) = offset {
        let r = t
            .authors
            .get_author(GetAuthorRequest {
                id: a.id.clone(),
                include_messages: true,
                messages_limit: Some(2),
                messages_offset: Some(o),
            })
            .await
            .unwrap()
            .into_inner();
        seen.extend(r.messages.iter().map(|m| m.id.clone()));
        offset = r.next_messages_offset;
        pages += 1;
        assert!(pages <= 3, "pagination does not terminate");
    }
    assert_eq!(seen, ids);
    assert_eq!(pages, 3);

    // exact multiple: 5 messages, limit 5 => no next page
    let full = t
        .authors
        .get_author(GetAuthorRequest {
            id: a.id.clone(),
            include_messages: true,
            messages_limit: Some(5),
            messages_offset: None,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(full.messages.len(), 5);
    assert_eq!(full.next_messages_offset, None);

    // limits validated even when messages are not requested
    let st = t
        .authors
        .get_author(GetAuthorRequest {
            id: "bad".into(),
            include_messages: false,
            messages_limit: Some(0),
            messages_offset: Some(-1),
        })
        .await
        .unwrap_err();
    let fields: Vec<_> = violations(&st).into_iter().map(|v| v.0).collect();
    assert_eq!(fields, ["id", "messages_limit", "messages_offset"]);

    assert_status(
        t.authors
            .get_author(GetAuthorRequest {
                id: random_uuid(),
                ..Default::default()
            })
            .await,
        Code::NotFound,
        "NOT_FOUND",
    );
}

#[tokio::test]
async fn update_author_partial_updates_and_conflicts() {
    let mut t = env!(CacheMode::Disabled);
    let a = t.author("Before").await;
    let b = t.author("Taken").await;
    let u = t
        .authors
        .update_author(UpdateAuthorRequest {
            id: a.id.clone(),
            name: Some("After".into()),
            email: None,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (u.name.as_str(), u.email.as_str()),
        ("After", a.email.as_str())
    );
    let u = t
        .authors
        .update_author(UpdateAuthorRequest {
            id: a.id.clone(),
            name: None,
            email: Some("new@example.org".into()),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (u.name.as_str(), u.email.as_str()),
        ("After", "new@example.org")
    );
    // no fields: returns the current row
    let same = t
        .authors
        .update_author(UpdateAuthorRequest {
            id: a.id.clone(),
            name: None,
            email: None,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(same, u);

    assert_status(
        t.authors
            .update_author(UpdateAuthorRequest {
                id: a.id.clone(),
                name: None,
                email: Some(b.email.clone()),
            })
            .await,
        Code::AlreadyExists,
        "CONFLICT",
    );
    assert_status(
        t.authors
            .update_author(UpdateAuthorRequest {
                id: random_uuid(),
                name: Some("x".into()),
                email: None,
            })
            .await,
        Code::NotFound,
        "NOT_FOUND",
    );
    assert_status(
        t.authors
            .update_author(UpdateAuthorRequest {
                id: a.id,
                name: Some(" ".into()),
                email: Some("bad".into()),
            })
            .await,
        Code::InvalidArgument,
        "BAD_USER_INPUT",
    );
}

#[tokio::test]
async fn delete_author_blocked_by_messages_then_allowed() {
    let mut t = env!(CacheMode::Disabled);
    let a = t.author("Busy").await;
    let m = t.message(&a, "t", "c").await;
    assert_status(
        t.authors
            .delete_author(DeleteAuthorRequest { id: a.id.clone() })
            .await,
        Code::FailedPrecondition,
        "CONFLICT",
    );
    t.messages
        .delete_message(DeleteMessageRequest { id: m.id })
        .await
        .unwrap();
    t.authors
        .delete_author(DeleteAuthorRequest { id: a.id.clone() })
        .await
        .unwrap();
    assert_status(
        t.authors
            .delete_author(DeleteAuthorRequest { id: a.id })
            .await,
        Code::NotFound,
        "NOT_FOUND",
    );
    assert_status(
        t.authors
            .delete_author(DeleteAuthorRequest { id: "?".into() })
            .await,
        Code::InvalidArgument,
        "BAD_USER_INPUT",
    );
}

// ---------------------------------------------------------------- Redis cache behaviour

#[tokio::test]
async fn get_populates_cache_with_protobuf_and_ttl_and_releases_lock() {
    let mut t = env!(CacheMode::Redis);
    let a = t.author("Cached").await;
    let m = t.message(&a, "T", "C").await;
    let id: Uuid = m.id.parse().unwrap();
    assert!(
        t.redis_get(&entry_key(id)).await.is_none(),
        "create must not populate the cache"
    );

    t.messages
        .get_message(GetMessageRequest { id: m.id.clone() })
        .await
        .unwrap();
    let raw = t
        .redis_get(&entry_key(id))
        .await
        .expect("entry cached after miss");
    let cached = Message::decode(raw.as_slice()).expect("entry is a protobuf Message");
    assert_eq!((cached.id.as_str(), cached.version), (m.id.as_str(), 0));
    let ttl = t.redis_ttl(&entry_key(id)).await;
    assert!(
        (1..=3600).contains(&ttl),
        "safety-net TTL expected, got {ttl}"
    );
    assert!(
        t.redis_get(&lock_key(id)).await.is_none(),
        "lock must be released"
    );
}

#[tokio::test]
async fn second_get_is_served_from_cache() {
    let mut t = env!(CacheMode::Redis);
    let a = t.author("Hit").await;
    let m = t.message(&a, "original", "C").await;
    t.messages
        .get_message(GetMessageRequest { id: m.id.clone() })
        .await
        .unwrap();
    // Change the row behind the service's back: only a cache hit still shows the old title.
    sqlx::query("UPDATE messages SET title = 'sneaky' WHERE id = $1::uuid")
        .bind(&m.id)
        .execute(&t.pool)
        .await
        .unwrap();
    let got = t
        .messages
        .get_message(GetMessageRequest { id: m.id.clone() })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got.title, "original");
    // ...and the cache hit is recorded.
    assert!(
        t.metrics
            .render()
            .contains("message_cache_seconds_count{operation=\"get\",outcome=\"hit\"}")
    );
}

#[tokio::test]
async fn cache_hit_carries_the_live_author() {
    let mut t = env!(CacheMode::Redis);
    let a = t.author("Before").await;
    let m = t.message(&a, "T", "C").await;
    t.messages
        .get_message(GetMessageRequest { id: m.id.clone() })
        .await
        .unwrap();
    t.authors
        .update_author(UpdateAuthorRequest {
            id: a.id.clone(),
            name: Some("After".into()),
            email: None,
        })
        .await
        .unwrap();
    let got = t
        .messages
        .get_message(GetMessageRequest { id: m.id })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got.author.unwrap().name, "After");
}

#[tokio::test]
async fn update_and_delete_evict_the_entry() {
    let mut t = env!(CacheMode::Redis);
    let a = t.author("Evict").await;
    let m = t.message(&a, "T", "C").await;
    let id: Uuid = m.id.parse().unwrap();
    t.messages
        .get_message(GetMessageRequest { id: m.id.clone() })
        .await
        .unwrap();
    assert!(t.redis_get(&entry_key(id)).await.is_some());

    t.messages
        .update_message(UpdateMessageRequest {
            id: m.id.clone(),
            title: Some("T2".into()),
            content: "C".into(),
            version: Some(0),
        })
        .await
        .unwrap();
    assert!(
        t.redis_get(&entry_key(id)).await.is_none(),
        "update must evict"
    );
    let fresh = t
        .messages
        .get_message(GetMessageRequest { id: m.id.clone() })
        .await
        .unwrap()
        .into_inner();
    assert_eq!((fresh.title.as_str(), fresh.version), ("T2", 1));
    assert!(t.redis_get(&entry_key(id)).await.is_some());

    t.messages
        .delete_message(DeleteMessageRequest { id: m.id.clone() })
        .await
        .unwrap();
    assert!(
        t.redis_get(&entry_key(id)).await.is_none(),
        "delete must evict"
    );
    assert!(t.redis_get(&lock_key(id)).await.is_none());
}

#[tokio::test]
async fn failed_stale_update_also_evicts() {
    let mut t = env!(CacheMode::Redis);
    let a = t.author("Evict2").await;
    let m = t.message(&a, "T", "C").await;
    let id: Uuid = m.id.parse().unwrap();
    t.messages
        .get_message(GetMessageRequest { id: m.id.clone() })
        .await
        .unwrap();
    let r = t
        .messages
        .update_message(UpdateMessageRequest {
            id: m.id.clone(),
            title: None,
            content: "x".into(),
            version: Some(99),
        })
        .await;
    assert_status(r, Code::Aborted, "CONFLICT");
    assert!(
        t.redis_get(&entry_key(id)).await.is_none(),
        "stale callers must not leave a stale entry"
    );
}

#[tokio::test]
async fn unreadable_entry_is_treated_as_a_miss_and_repaired() {
    let mut t = env!(CacheMode::Redis);
    let a = t.author("Corrupt").await;
    let m = t.message(&a, "T", "C").await;
    let id: Uuid = m.id.parse().unwrap();
    t.redis_set(&entry_key(id), &[0xff, 0xff, 0xff], None).await;
    let got = t
        .messages
        .get_message(GetMessageRequest { id: m.id.clone() })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got, m);
    let raw = t.redis_get(&entry_key(id)).await.unwrap();
    assert_eq!(Message::decode(raw.as_slice()).unwrap().id, m.id);
}

#[tokio::test]
async fn deleted_row_with_stale_entry_is_never_served_after_delete() {
    let mut t = env!(CacheMode::Redis);
    let a = t.author("Ghost").await;
    let m = t.message(&a, "T", "C").await;
    t.messages
        .get_message(GetMessageRequest { id: m.id.clone() })
        .await
        .unwrap();
    t.messages
        .delete_message(DeleteMessageRequest { id: m.id.clone() })
        .await
        .unwrap();
    assert_status(
        t.messages.get_message(GetMessageRequest { id: m.id }).await,
        Code::NotFound,
        "NOT_FOUND",
    );
}

/// A lock held by someone else for longer than the wait: the read still succeeds from the DB
/// (lock timeout degrades, it does not fail the request) and the entry is NOT cached, because
/// without the lock a concurrent writer could be racing the fill.
#[tokio::test]
async fn lock_timeout_degrades_to_uncached_db_read() {
    let mut t = env!(CacheMode::Redis);
    let a = t.author("Locked").await;
    let m = t.message(&a, "T", "C").await;
    let id: Uuid = m.id.parse().unwrap();
    t.redis_set(&lock_key(id), b"someone-else", Some(30_000))
        .await;
    let started = Instant::now();
    let got = t
        .messages
        .get_message(GetMessageRequest { id: m.id.clone() })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got, m);
    assert!(
        started.elapsed() >= Duration::from_secs(4),
        "should have waited for the lock first"
    );
    assert!(t.redis_get(&entry_key(id)).await.is_none());
    // our release must not delete somebody else's lock
    assert_eq!(
        t.redis_get(&lock_key(id)).await.as_deref(),
        Some(&b"someone-else"[..])
    );
    t.redis_del(&lock_key(id)).await;
}

#[tokio::test]
async fn every_rpc_works_when_redis_is_unreachable() {
    let mut t = env!(CacheMode::Broken);
    let started = Instant::now();
    let a = t.author("NoRedis").await;
    let m = t.message(&a, "T", "C").await;
    let got = t
        .messages
        .get_message(GetMessageRequest { id: m.id.clone() })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got, m);
    let u = t
        .messages
        .update_message(UpdateMessageRequest {
            id: m.id.clone(),
            title: None,
            content: "C2".into(),
            version: Some(0),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(u.version, 1);
    t.messages
        .list_messages(ListMessagesRequest::default())
        .await
        .unwrap();
    t.messages
        .delete_message(DeleteMessageRequest { id: m.id })
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "cache failures must fail fast"
    );
    assert!(t.metrics.render().contains("message_cache_errors_total"));
}

#[tokio::test]
async fn every_rpc_works_with_the_cache_disabled() {
    let mut t = env!(CacheMode::Disabled);
    let a = t.author("Plain").await;
    let m = t.message(&a, "T", "C").await;
    let got = t
        .messages
        .get_message(GetMessageRequest { id: m.id.clone() })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got, m);
}

// ---------------------------------------------------------------- Metrics

#[tokio::test]
async fn per_rpc_metrics_are_recorded_with_status_and_error_code() {
    let mut t = env!(CacheMode::Disabled);
    let a = t.author("Metrics").await;
    t.messages
        .get_message(GetMessageRequest { id: random_uuid() })
        .await
        .unwrap_err();
    t.messages
        .get_message(GetMessageRequest { id: "bad".into() })
        .await
        .unwrap_err();
    let _ = a;
    let text = t.metrics.render();
    assert!(
        text.contains("grpc_server_seconds_count{grpc_status_code=\"OK\",rpc_method=\"CreateAuthor\",rpc_service=\"message.v1.AuthorService\"} 1"),
        "{text}"
    );
    assert!(
        text.contains("grpc_status_code=\"NOT_FOUND\",rpc_method=\"GetMessage\""),
        "{text}"
    );
    assert!(text.contains("grpc_errors_total{"), "{text}");
    assert!(text.contains("error_code=\"BAD_USER_INPUT\""), "{text}");
    assert!(text.contains("error_code=\"NOT_FOUND\""), "{text}");
}
