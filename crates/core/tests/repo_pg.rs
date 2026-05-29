//! Integration tests for the M2 repo layer against a real `PostgreSQL`.
//!
//! `#[sqlx::test]` creates a fresh database per test (using `DATABASE_URL`
//! as the template connection) and runs all `migrations/` before invoking
//! the test. To run them locally:
//!
//! ```bash
//! docker compose -f docker-compose.yml -f docker-compose.dev.yml up -d postgres
//! DATABASE_URL='postgres://sakurasato:devpassword@127.0.0.1:5432/sakurasato' \
//!     cargo test -p sakurasato-core --test repo_pg
//! ```
//!
//! CI provides the same env via a `services.postgres` block.

#![forbid(unsafe_code)]

use sakurasato_core::model::FollowState;
use sakurasato_core::repo;
use sqlx::PgPool;

fn sample_local_actor(suffix: &str) -> repo::actor::NewActor {
    repo::actor::NewActor {
        ap_id: format!("https://example.test/users/alice{suffix}"),
        preferred_username: format!("alice{suffix}"),
        host: "example.test".into(),
        display_name: Some("Alice".into()),
        summary: Some("hello fediverse".into()),
        icon_url: None,
        image_url: None,
        inbox_url: format!("https://example.test/users/alice{suffix}/inbox"),
        shared_inbox_url: Some("https://example.test/inbox".into()),
        outbox_url: Some(format!("https://example.test/users/alice{suffix}/outbox")),
        followers_url: None,
        following_url: None,
        public_key_id: format!("https://example.test/users/alice{suffix}#main-key"),
        public_key_pem: "-----BEGIN PUBLIC KEY-----\nMOCK\n-----END PUBLIC KEY-----".into(),
        private_key_pem: Some(
            "-----BEGIN PRIVATE KEY-----\nMOCK\n-----END PRIVATE KEY-----".into(),
        ),
        also_known_as: vec![format!("https://old.example.test/users/alice{suffix}")],
        moved_to_ap_id: None,
        is_local: true,
        actor_type: "Person".into(),
    }
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn actor_round_trip(pool: PgPool) -> sqlx::Result<()> {
    let inserted = repo::actor::insert(&pool, sample_local_actor("1")).await?;
    assert!(inserted.id > 0);
    assert_eq!(inserted.preferred_username, "alice1");
    assert_eq!(inserted.also_known_as.0.len(), 1);

    let by_id = repo::actor::get_by_id(&pool, inserted.id).await?.unwrap();
    assert_eq!(by_id.ap_id, inserted.ap_id);

    let by_ap_id = repo::actor::get_by_ap_id(&pool, &inserted.ap_id)
        .await?
        .unwrap();
    assert_eq!(by_ap_id.id, inserted.id);

    let by_username = repo::actor::get_by_username_host(&pool, "alice1", "example.test")
        .await?
        .unwrap();
    assert_eq!(by_username.id, inserted.id);

    assert!(by_id.fetched_at.is_none());
    repo::actor::mark_fetched(&pool, inserted.id).await?;
    let refetched = repo::actor::get_by_id(&pool, inserted.id).await?.unwrap();
    assert!(refetched.fetched_at.is_some());

    // 秘密鍵が Debug / Serialize 経由で漏れないことを担保する (M2 レビュー指摘)。
    let debug_repr = format!("{refetched:?}");
    assert!(
        debug_repr.contains("private_key_pem: Some(\"<redacted>\")"),
        "private_key_pem must be redacted in Debug, got: {debug_repr}"
    );
    assert!(
        !debug_repr.contains("BEGIN PRIVATE KEY"),
        "private key body leaked into Debug: {debug_repr}"
    );

    let json = serde_json::to_string(&refetched).unwrap();
    assert!(
        !json.contains("private_key_pem"),
        "private_key_pem must be skipped in Serialize, got: {json}"
    );
    assert!(
        !json.contains("BEGIN PRIVATE KEY"),
        "private key body leaked into JSON: {json}"
    );

    Ok(())
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn note_round_trip(pool: PgPool) -> sqlx::Result<()> {
    let author = repo::actor::insert(&pool, sample_local_actor("2")).await?;
    let now = chrono::Utc::now();
    let new = repo::note::NewNote {
        ap_id: "https://example.test/notes/n1".into(),
        actor_id: author.id,
        content: "<p>はじめてのトート。</p>".into(),
        language: Some("ja".into()),
        in_reply_to_ap_id: None,
        in_reply_to_note_id: None,
        summary: None,
        visibility: "public".into(),
        sensitive: false,
        to_recipients: vec!["https://www.w3.org/ns/activitystreams#Public".into()],
        cc_recipients: vec![format!("{}/followers", author.ap_id)],
        attachments: serde_json::json!([]),
        tags: serde_json::json!([]),
        is_local: true,
        url: Some("https://example.test/@alice2/n1".into()),
        published_at: now,
    };
    let inserted = repo::note::insert(&pool, new).await?;
    assert_eq!(inserted.actor_id, author.id);
    assert_eq!(inserted.to_recipients.0.len(), 1);

    let fetched = repo::note::get_by_ap_id(&pool, &inserted.ap_id)
        .await?
        .unwrap();
    assert_eq!(fetched.content, inserted.content);
    Ok(())
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn follow_state_transitions(pool: PgPool) -> sqlx::Result<()> {
    let me = repo::actor::insert(&pool, sample_local_actor("a")).await?;
    let them = repo::actor::insert(&pool, sample_local_actor("b")).await?;

    let follow =
        repo::follow::insert_pending(&pool, "https://example.test/follows/f1", me.id, them.id)
            .await?;
    assert_eq!(follow.state, FollowState::Pending.as_str());

    repo::follow::set_state(&pool, follow.id, FollowState::Accepted).await?;
    let updated = repo::follow::get_by_ap_id(&pool, &follow.ap_id)
        .await?
        .unwrap();
    assert_eq!(updated.state, "accepted");
    assert!(updated.updated_at >= follow.updated_at);
    Ok(())
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn delivery_queue_enqueue_and_fail(pool: PgPool) -> sqlx::Result<()> {
    let sender = repo::actor::insert(&pool, sample_local_actor("d")).await?;
    let payload = serde_json::json!({"type": "Create", "actor": sender.ap_id});
    let row =
        repo::delivery_queue::enqueue(&pool, "https://remote.example/inbox", &payload, sender.id)
            .await?;
    assert_eq!(row.attempts, 0);
    assert_eq!(row.state, "pending");

    let next = chrono::Utc::now() + chrono::Duration::seconds(60);
    repo::delivery_queue::mark_failed(&pool, row.id, "503 Service Unavailable", next).await?;
    let after = repo::delivery_queue::get_by_id(&pool, row.id)
        .await?
        .unwrap();
    assert_eq!(after.attempts, 1);
    assert_eq!(after.state, "failed");
    assert_eq!(after.last_error.as_deref(), Some("503 Service Unavailable"));
    Ok(())
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn emoji_upsert_overwrites_by_shortcode(pool: PgPool) -> sqlx::Result<()> {
    let first = repo::emoji::upsert_local(
        &pool,
        repo::emoji::NewLocalEmoji {
            shortcode: "blob_party".into(),
            category: Some("blob".into()),
            aliases: vec!["party".into()],
            image_key: "emoji/local/blob_party.png".into(),
            media_type: "image/png".into(),
        },
    )
    .await?;
    assert_eq!(first.category.as_deref(), Some("blob"));

    // 同 shortcode を別 category で再 upsert → Misskey インポート互換 (上書き)。
    let second = repo::emoji::upsert_local(
        &pool,
        repo::emoji::NewLocalEmoji {
            shortcode: "blob_party".into(),
            category: Some("celebration".into()),
            aliases: vec!["party".into(), "tada".into()],
            image_key: "emoji/local/blob_party.webp".into(),
            media_type: "image/webp".into(),
        },
    )
    .await?;
    assert_eq!(first.id, second.id, "same row, just updated");
    assert_eq!(second.category.as_deref(), Some("celebration"));
    assert_eq!(second.aliases.0, vec!["party", "tada"]);
    assert_eq!(second.media_type, "image/webp");
    Ok(())
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn reaction_insert_and_delete(pool: PgPool) -> sqlx::Result<()> {
    let author = repo::actor::insert(&pool, sample_local_actor("r1")).await?;
    let reactor = repo::actor::insert(&pool, sample_local_actor("r2")).await?;
    let note = repo::note::insert(
        &pool,
        repo::note::NewNote {
            ap_id: "https://example.test/notes/nr".into(),
            actor_id: author.id,
            content: "hi".into(),
            language: None,
            in_reply_to_ap_id: None,
            in_reply_to_note_id: None,
            summary: None,
            visibility: "public".into(),
            sensitive: false,
            to_recipients: vec![],
            cc_recipients: vec![],
            attachments: serde_json::json!([]),
            tags: serde_json::json!([]),
            is_local: true,
            url: None,
            published_at: chrono::Utc::now(),
        },
    )
    .await?;
    let r = repo::reaction::insert(
        &pool,
        "https://example.test/reactions/x",
        note.id,
        reactor.id,
        "👍",
        None,
    )
    .await?;
    assert_eq!(r.content, "👍");

    let deleted = repo::reaction::delete_by_ap_id(&pool, &r.ap_id).await?;
    assert_eq!(deleted, 1);
    Ok(())
}
