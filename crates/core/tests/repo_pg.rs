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

use sakurasato_core::model::{FollowState, Visibility};
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
        // M3b: Ed25519 鍵を併載する。core レイヤは PEM をパースせず TEXT として
        // 保存するだけなので MOCK で round trip を回せる (multibase 変換は
        // sakurasato-server の actor JSON テスト側で本物の鍵を使う)。
        ed25519_public_key_id: Some(format!(
            "https://example.test/users/alice{suffix}#ed25519-key"
        )),
        ed25519_public_key_pem: Some(
            "-----BEGIN PUBLIC KEY-----\nMOCK-ED\n-----END PUBLIC KEY-----".into(),
        ),
        ed25519_private_key_pem: Some(
            "-----BEGIN PRIVATE KEY-----\nMOCK-ED-PRIV\n-----END PRIVATE KEY-----".into(),
        ),
        also_known_as: vec![format!("https://old.example.test/users/alice{suffix}")],
        moved_to_ap_id: None,
        is_local: true,
        actor_type: "Person".into(),
        manually_approves_followers: false,
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

    // Ed25519 鍵が round trip し、actor JSON 側で読める形で保存されているこ
    // とを確認する。
    assert_eq!(
        refetched.ed25519_public_key_id.as_deref(),
        Some("https://example.test/users/alice1#ed25519-key"),
    );
    assert!(refetched.ed25519_public_key_pem.is_some());
    assert!(refetched.ed25519_private_key_pem.is_some());

    // 秘密鍵 (RSA + Ed25519 両方) が Debug / Serialize 経由で漏れないことを
    // 担保する (M2 レビュー指摘 + M3b で Ed25519 にも同じ保護を拡張)。
    let debug_repr = format!("{refetched:?}");
    assert!(
        debug_repr.contains("private_key_pem: Some(\"<redacted>\")"),
        "private_key_pem must be redacted in Debug, got: {debug_repr}"
    );
    assert!(
        debug_repr.contains("ed25519_private_key_pem: Some(\"<redacted>\")"),
        "ed25519_private_key_pem must be redacted in Debug, got: {debug_repr}"
    );
    assert!(
        !debug_repr.contains("BEGIN PRIVATE KEY"),
        "private key body leaked into Debug: {debug_repr}"
    );
    assert!(
        !debug_repr.contains("MOCK-ED-PRIV"),
        "Ed25519 private key body leaked into Debug: {debug_repr}"
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
    assert!(
        !json.contains("MOCK-ED-PRIV"),
        "Ed25519 private key body leaked into JSON: {json}"
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
        visibility: Visibility::Public,
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
async fn upsert_pending_recovers_from_rejected_with_new_ap_id(pool: PgPool) -> sqlx::Result<()> {
    // PR #80 round-2 fix (#2): rejected 行が `(follower, followed)` UNIQUE
    // で残っている状態で「同じ follower → 別の ap_id の Follow」が来たとき、
    // ap_id を新しい値に書き換え、state を `pending` にリセットする。
    // これをやらないと 1 度 reject した相手は永久に再 follow できなくなる。
    let me = repo::actor::insert(&pool, sample_local_actor("me")).await?;
    let them = repo::actor::insert(&pool, sample_local_actor("them")).await?;

    // 旧 Follow → reject
    let old =
        repo::follow::insert_pending(&pool, "https://example.test/follows/old", them.id, me.id)
            .await?;
    repo::follow::set_state(&pool, old.id, FollowState::Rejected).await?;

    // 新しい ap_id で再 Follow が届く (= リモートが Unfollow → 再 Follow)
    let new =
        repo::follow::upsert_pending(&pool, "https://example.test/follows/new", them.id, me.id)
            .await?;

    // 同じ id (ON CONFLICT) かつ pending に戻り、ap_id が新しい値で上書きされる。
    assert_eq!(new.id, old.id, "ON CONFLICT must reuse the same row");
    assert_eq!(new.state, FollowState::Pending.as_str());
    assert_eq!(new.ap_id, "https://example.test/follows/new");
    Ok(())
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn upsert_pending_keeps_accepted_state_on_retry(pool: PgPool) -> sqlx::Result<()> {
    // 既存 accepted の retry は accepted のまま据え置く (= Mastodon の retry
    // で勝手に pending に巻き戻さない)。ap_id も既存値を保つ。
    let me = repo::actor::insert(&pool, sample_local_actor("me2")).await?;
    let them = repo::actor::insert(&pool, sample_local_actor("them2")).await?;

    let accepted =
        repo::follow::insert_pending(&pool, "https://example.test/follows/keep", them.id, me.id)
            .await?;
    repo::follow::set_state(&pool, accepted.id, FollowState::Accepted).await?;

    // 別 ap_id で再到達 — accepted を保持し ap_id は触らない。
    let again =
        repo::follow::upsert_pending(&pool, "https://example.test/follows/other", them.id, me.id)
            .await?;
    assert_eq!(again.id, accepted.id);
    assert_eq!(again.state, FollowState::Accepted.as_str());
    assert_eq!(again.ap_id, "https://example.test/follows/keep");
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
    repo::delivery_queue::mark_failed(
        &pool,
        row.id,
        "503 Service Unavailable",
        next,
        repo::delivery_queue::DEFAULT_MAX_ATTEMPTS,
    )
    .await?;
    let after = repo::delivery_queue::get_by_id(&pool, row.id)
        .await?
        .unwrap();
    assert_eq!(after.attempts, 1);
    assert_eq!(after.state, "failed");
    assert_eq!(after.last_error.as_deref(), Some("503 Service Unavailable"));
    Ok(())
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn delivery_queue_transitions_to_dead_at_max_attempts(pool: PgPool) -> sqlx::Result<()> {
    let sender = repo::actor::insert(&pool, sample_local_actor("dead")).await?;
    let row = repo::delivery_queue::enqueue(
        &pool,
        "https://gone.example/inbox",
        &serde_json::json!({"type": "Create"}),
        sender.id,
    )
    .await?;
    let later = chrono::Utc::now() + chrono::Duration::seconds(60);
    // max_attempts=3 で 3 回失敗させる → 3 回目で dead 遷移。
    for _ in 0..3 {
        repo::delivery_queue::mark_failed(&pool, row.id, "boom", later, 3).await?;
    }
    let after = repo::delivery_queue::get_by_id(&pool, row.id)
        .await?
        .unwrap();
    assert_eq!(after.attempts, 3);
    assert_eq!(
        after.state, "dead",
        "must retire to dead after max_attempts"
    );

    // 既に dead の行は mark_failed を呼んでも変わらないことを担保 (F-2)。
    repo::delivery_queue::mark_failed(&pool, row.id, "ignored", later, 3).await?;
    let untouched = repo::delivery_queue::get_by_id(&pool, row.id)
        .await?
        .unwrap();
    assert_eq!(untouched.attempts, 3, "dead row must not be resurrected");
    assert_eq!(untouched.state, "dead");
    assert_eq!(
        untouched.last_error.as_deref(),
        Some("boom"),
        "last_error stays from the dead transition"
    );

    // delivered 行も mark_failed では巻き戻されない (M2 4th review F-1)。
    let happy = repo::delivery_queue::enqueue(
        &pool,
        "https://ok.example/inbox",
        &serde_json::json!({"type": "Create"}),
        sender.id,
    )
    .await?;
    sqlx::query!(
        "UPDATE delivery_queue SET state = 'delivered' WHERE id = $1",
        happy.id
    )
    .execute(&pool)
    .await?;
    repo::delivery_queue::mark_failed(&pool, happy.id, "stale call", later, 3).await?;
    let still_delivered = repo::delivery_queue::get_by_id(&pool, happy.id)
        .await?
        .unwrap();
    assert_eq!(still_delivered.state, "delivered");
    assert_eq!(still_delivered.attempts, 0);
    assert!(still_delivered.last_error.is_none());
    Ok(())
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn delivery_queue_mark_delivered_terminates_row(pool: PgPool) -> sqlx::Result<()> {
    let sender = repo::actor::insert(&pool, sample_local_actor("ok")).await?;
    let row = repo::delivery_queue::enqueue(
        &pool,
        "https://ok.example/inbox",
        &serde_json::json!({"type": "Create"}),
        sender.id,
    )
    .await?;

    // 一度失敗 → 失敗状態でも mark_delivered で確定できる (ワーカが N 回目で
    // 成功するケース)。
    let later = chrono::Utc::now() + chrono::Duration::seconds(60);
    repo::delivery_queue::mark_failed(&pool, row.id, "temp glitch", later, 5).await?;
    repo::delivery_queue::mark_delivered(&pool, row.id).await?;
    let done = repo::delivery_queue::get_by_id(&pool, row.id)
        .await?
        .unwrap();
    assert_eq!(done.state, "delivered");
    assert_eq!(done.attempts, 2, "attempts incremented for the success try");
    assert!(
        done.last_error.is_none(),
        "last_error must be cleared on delivered"
    );

    // 終端状態への二重コールはノーオプ (べき等)。
    repo::delivery_queue::mark_delivered(&pool, row.id).await?;
    let still = repo::delivery_queue::get_by_id(&pool, row.id)
        .await?
        .unwrap();
    assert_eq!(still.attempts, 2, "delivered row must not be re-attempted");
    Ok(())
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn delivery_queue_mark_delivered_skips_dead_rows(pool: PgPool) -> sqlx::Result<()> {
    // dead に倒したあとに何らかの理由で mark_delivered が誤って呼ばれても、
    // 状態を巻き戻さないこと (M2 4th review F-1 と対称な保証)。
    let sender = repo::actor::insert(&pool, sample_local_actor("z")).await?;
    let row = repo::delivery_queue::enqueue(
        &pool,
        "https://dead.example/inbox",
        &serde_json::json!({"type": "Create"}),
        sender.id,
    )
    .await?;
    let later = chrono::Utc::now() + chrono::Duration::seconds(60);
    for _ in 0..2 {
        repo::delivery_queue::mark_failed(&pool, row.id, "boom", later, 2).await?;
    }
    let dead = repo::delivery_queue::get_by_id(&pool, row.id)
        .await?
        .unwrap();
    assert_eq!(dead.state, "dead");

    repo::delivery_queue::mark_delivered(&pool, row.id).await?;
    let still_dead = repo::delivery_queue::get_by_id(&pool, row.id)
        .await?
        .unwrap();
    assert_eq!(still_dead.state, "dead", "dead row must not be revived");
    Ok(())
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn delivery_queue_mark_dead_transitions_immediately(pool: PgPool) -> sqlx::Result<()> {
    // 永続エラー (signing / SSRF / serialize) は M3b-3 round-2 F1 で即時 `dead`
    // 遷移するようにした。pending 行と failed 行の両方で動作することを確認。
    let sender = repo::actor::insert(&pool, sample_local_actor("perm")).await?;

    // pending → dead 直行 (attempts は触らない)。
    let row = repo::delivery_queue::enqueue(
        &pool,
        "https://localhost/inbox",
        &serde_json::json!({"type": "Create"}),
        sender.id,
    )
    .await?;
    repo::delivery_queue::mark_dead(&pool, row.id, "ssrf: localhost-domain").await?;
    let dead = repo::delivery_queue::get_by_id(&pool, row.id)
        .await?
        .unwrap();
    assert_eq!(dead.state, "dead");
    assert_eq!(dead.attempts, 0, "mark_dead must not increment attempts");
    assert_eq!(dead.last_error.as_deref(), Some("ssrf: localhost-domain"));

    // failed 状態でも dead に倒せる (一時失敗を経て永続原因が判明したケース)。
    let row2 = repo::delivery_queue::enqueue(
        &pool,
        "https://retry.example/inbox",
        &serde_json::json!({"type": "Create"}),
        sender.id,
    )
    .await?;
    let later = chrono::Utc::now() + chrono::Duration::seconds(60);
    repo::delivery_queue::mark_failed(&pool, row2.id, "transport", later, 5).await?;
    repo::delivery_queue::mark_dead(&pool, row2.id, "permanent later").await?;
    let dead2 = repo::delivery_queue::get_by_id(&pool, row2.id)
        .await?
        .unwrap();
    assert_eq!(dead2.state, "dead");
    assert_eq!(dead2.attempts, 1, "earlier mark_failed の attempts は維持");
    assert_eq!(dead2.last_error.as_deref(), Some("permanent later"));

    // delivered 行は mark_dead で巻き戻されない (終端状態保護)。
    let happy = repo::delivery_queue::enqueue(
        &pool,
        "https://ok.example/inbox",
        &serde_json::json!({"type": "Create"}),
        sender.id,
    )
    .await?;
    repo::delivery_queue::mark_delivered(&pool, happy.id).await?;
    repo::delivery_queue::mark_dead(&pool, happy.id, "should not apply").await?;
    let still_delivered = repo::delivery_queue::get_by_id(&pool, happy.id)
        .await?
        .unwrap();
    assert_eq!(
        still_delivered.state, "delivered",
        "delivered row must not be revived to dead"
    );
    assert!(still_delivered.last_error.is_none());
    Ok(())
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn emoji_upsert_rejects_invalid_shortcode(pool: PgPool) -> sqlx::Result<()> {
    // Issue #188: 長さ上限が 64 → 128 に緩和されたので、不正ケースは「文字種違反」
    // と「129 chars (= 新上限超え)」に更新する。
    let too_long_129 = "a".repeat(129);
    let cases: [&str; 5] = [
        "../escape",
        "with space",
        "コロン",
        "",
        // 129 chars (= 新上限 128 を 1 つ超える)
        too_long_129.as_str(),
    ];
    for bad in cases {
        let err = repo::emoji::upsert_local(
            &pool,
            repo::emoji::NewLocalEmoji {
                shortcode: bad.into(),
                category: None,
                aliases: vec![],
                image_key: "x".into(),
                media_type: "image/png".into(),
            },
        )
        .await
        .err()
        .unwrap_or_else(|| panic!("expected error for shortcode {bad:?}"));
        let msg = format!("{err}");
        assert!(
            msg.contains("invalid emoji shortcode"),
            "wrong error for {bad:?}: {msg}"
        );
    }
    Ok(())
}

/// Issue #188: 旧上限 (64) を超える 65〜128 chars の shortcode が DB CHECK
/// 制約 (= 新 migration `0017_emoji_shortcode_128`) を通って **受理** される
/// ことを確認する境界回帰テスト。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn emoji_upsert_accepts_shortcode_up_to_128_chars(pool: PgPool) -> sqlx::Result<()> {
    for len in [65_usize, 100, 128] {
        let shortcode = "a".repeat(len);
        let row = repo::emoji::upsert_local(
            &pool,
            repo::emoji::NewLocalEmoji {
                shortcode: shortcode.clone(),
                category: None,
                aliases: vec![],
                image_key: format!("emoji/local/{shortcode}.webp"),
                media_type: "image/webp".into(),
            },
        )
        .await
        .unwrap_or_else(|err| panic!("len {len} should be accepted, got error: {err}"));
        assert_eq!(row.shortcode, shortcode);
    }
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
            visibility: Visibility::Public,
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

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn reaction_insert_or_get_is_idempotent(pool: PgPool) -> sqlx::Result<()> {
    let author = repo::actor::insert(&pool, sample_local_actor("io1")).await?;
    let reactor = repo::actor::insert(&pool, sample_local_actor("io2")).await?;
    let note = repo::note::insert(
        &pool,
        repo::note::NewNote {
            ap_id: "https://example.test/notes/io".into(),
            actor_id: author.id,
            content: "hi".into(),
            language: None,
            in_reply_to_ap_id: None,
            in_reply_to_note_id: None,
            summary: None,
            visibility: Visibility::Public,
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
    // 同じ ap_id を 2 回 → 同じ行が返る (= リトライ安全)。
    let a =
        repo::reaction::insert_or_get(&pool, "https://x.test/r/a", note.id, reactor.id, "👍", None)
            .await?;
    let b =
        repo::reaction::insert_or_get(&pool, "https://x.test/r/a", note.id, reactor.id, "👍", None)
            .await?;
    assert_eq!(a.id, b.id);

    // 別 ap_id だが natural key (note, actor, content) が衝突 → 既存行を返す。
    let c =
        repo::reaction::insert_or_get(&pool, "https://x.test/r/b", note.id, reactor.id, "👍", None)
            .await?;
    assert_eq!(a.id, c.id);
    // 既存行が返るので ap_id は最初のもの (= "/r/a") のまま。
    assert_eq!(c.ap_id, "https://x.test/r/a");

    // 別 content なら新規行。
    let d =
        repo::reaction::insert_or_get(&pool, "https://x.test/r/c", note.id, reactor.id, "🎉", None)
            .await?;
    assert_ne!(a.id, d.id);
    Ok(())
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn reaction_count_by_note_groups_by_content(pool: PgPool) -> sqlx::Result<()> {
    let author = repo::actor::insert(&pool, sample_local_actor("ct1")).await?;
    let r1 = repo::actor::insert(&pool, sample_local_actor("ct2")).await?;
    let r2 = repo::actor::insert(&pool, sample_local_actor("ct3")).await?;
    let note = repo::note::insert(
        &pool,
        repo::note::NewNote {
            ap_id: "https://example.test/notes/ct".into(),
            actor_id: author.id,
            content: "hi".into(),
            language: None,
            in_reply_to_ap_id: None,
            in_reply_to_note_id: None,
            summary: None,
            visibility: Visibility::Public,
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
    repo::reaction::insert(&pool, "https://x.test/r/1", note.id, r1.id, "👍", None).await?;
    repo::reaction::insert(&pool, "https://x.test/r/2", note.id, r2.id, "👍", None).await?;
    repo::reaction::insert(&pool, "https://x.test/r/3", note.id, r1.id, "🎉", None).await?;

    let counts = repo::reaction::count_by_note(&pool, note.id).await?;
    // 👍 が 2 件、🎉 が 1 件。ordering は MIN(created_at) で第一に挿入したもの順。
    assert_eq!(counts.len(), 2);
    assert_eq!(counts[0].content, "👍");
    assert_eq!(counts[0].count, 2);
    assert_eq!(counts[1].content, "🎉");
    assert_eq!(counts[1].count, 1);
    Ok(())
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn emoji_upsert_remote_is_idempotent_by_ap_id(pool: PgPool) -> sqlx::Result<()> {
    // Issue #135 で `image_key` を `Option<String>` に倒し、Issue #192 で
    // SQL を COALESCE 保存に切り替えたため、同じ ap_id を再投入したときの
    // 挙動マトリクスをここで固定する。
    //
    // ## (1) 新規 success → image_key=Some, last_failed_at=None
    let first = repo::emoji::upsert_remote(
        &pool,
        repo::emoji::NewRemoteEmoji {
            shortcode: "blob".into(),
            ap_id: "https://misskey.io/emojis/blob".into(),
            host: "misskey.io".into(),
            image_key: Some("emoji/remote/misskey.io/blob.webp".into()),
            media_type: "image/webp".into(),
            last_failed_at: None,
        },
    )
    .await?;
    assert!(!first.is_local);
    assert_eq!(first.host.as_deref(), Some("misskey.io"));
    assert_eq!(
        first.image_key.as_deref(),
        Some("emoji/remote/misskey.io/blob.webp")
    );
    assert!(first.last_failed_at.is_none());

    // ## (2) 既存 failure → image_key は **既存値を温存**、last_failed_at が
    //       now() で更新される (= COALESCE による保存)。
    let now = chrono::Utc::now();
    let second = repo::emoji::upsert_remote(
        &pool,
        repo::emoji::NewRemoteEmoji {
            shortcode: "blob".into(),
            ap_id: "https://misskey.io/emojis/blob".into(),
            host: "misskey.io".into(),
            image_key: None,
            media_type: "application/octet-stream".into(),
            last_failed_at: Some(now),
        },
    )
    .await?;
    assert_eq!(first.id, second.id);
    // 既存 image_key (= 自鯖キャッシュキー) が温存される。
    assert_eq!(
        second.image_key.as_deref(),
        Some("emoji/remote/misskey.io/blob.webp"),
    );
    // media_type も既存値が温存される (= 失敗時に巻き戻らない)。
    assert_eq!(second.media_type, "image/webp");
    assert!(second.last_failed_at.is_some());

    // ## (3) 既存 success (retry 成功) → image_key を新値で上書き + last_failed_at=None
    let third = repo::emoji::upsert_remote(
        &pool,
        repo::emoji::NewRemoteEmoji {
            shortcode: "blob".into(),
            ap_id: "https://misskey.io/emojis/blob".into(),
            host: "misskey.io".into(),
            image_key: Some("emoji/remote/misskey.io/blob.webp".into()),
            media_type: "image/webp".into(),
            last_failed_at: None,
        },
    )
    .await?;
    assert_eq!(first.id, third.id);
    assert!(third.last_failed_at.is_none(), "success should reset");

    // get_by_ap_id でも引ける。
    let got = repo::emoji::get_by_ap_id(&pool, "https://misskey.io/emojis/blob").await?;
    assert_eq!(got.map(|r| r.id), Some(first.id));
    Ok(())
}

/// Issue #192 round-2 regression #1: 旧 URL を持つ row が fetch 失敗で
/// `image_key = NULL` に降格しないことを担保する。COALESCE 保存の核心テスト。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn emoji_upsert_remote_failure_preserves_legacy_url(pool: PgPool) -> sqlx::Result<()> {
    // PR #191 前の挙動を模して、image_key に URL を持つ row を作る。
    let legacy = repo::emoji::upsert_remote(
        &pool,
        repo::emoji::NewRemoteEmoji {
            shortcode: "old".into(),
            ap_id: "https://old.example/emojis/old".into(),
            host: "old.example".into(),
            image_key: Some("https://old.example/files/old.png".into()),
            media_type: "image/png".into(),
            last_failed_at: None,
        },
    )
    .await?;
    assert_eq!(
        legacy.image_key.as_deref(),
        Some("https://old.example/files/old.png")
    );

    // 同じ ap_id で fetch failure を模す (= image_key=None, last_failed_at=now)。
    let after_fail = repo::emoji::upsert_remote(
        &pool,
        repo::emoji::NewRemoteEmoji {
            shortcode: "old".into(),
            ap_id: "https://old.example/emojis/old".into(),
            host: "old.example".into(),
            image_key: None,
            media_type: "application/octet-stream".into(),
            last_failed_at: Some(chrono::Utc::now()),
        },
    )
    .await?;
    assert_eq!(after_fail.id, legacy.id);
    // 旧 URL が温存される (= ここが PR #191 round-2 #1 の regression 修正点)。
    assert_eq!(
        after_fail.image_key.as_deref(),
        Some("https://old.example/files/old.png"),
        "fetch failure must NOT downgrade legacy URL row to NULL"
    );
    assert_eq!(after_fail.media_type, "image/png");
    assert!(after_fail.last_failed_at.is_some());
    Ok(())
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn actor_set_also_known_as_and_moved_to(pool: PgPool) -> sqlx::Result<()> {
    // M9: alsoKnownAs / moved_to_ap_id を後から書き換える経路の round-trip。
    let me = repo::actor::insert(&pool, sample_local_actor("aka")).await?;
    // sample_local_actor は alsoKnownAs に 1 件入れている前提。
    assert_eq!(me.also_known_as.0.len(), 1);
    assert!(me.moved_to_ap_id.is_none());

    let new_list = vec![
        "https://old.example.test/users/alice-old".to_string(),
        "https://another.example.test/users/alice".to_string(),
    ];
    let updated = repo::actor::set_also_known_as(&pool, me.id, &new_list).await?;
    assert_eq!(updated.also_known_as.0, new_list);

    // moved_to_ap_id: 立てる → 解除。
    let with_target =
        repo::actor::set_moved_to(&pool, me.id, Some("https://new.example/users/alice")).await?;
    assert_eq!(
        with_target.moved_to_ap_id.as_deref(),
        Some("https://new.example/users/alice"),
    );
    let cleared = repo::actor::set_moved_to(&pool, me.id, None).await?;
    assert!(cleared.moved_to_ap_id.is_none());
    Ok(())
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn follow_list_local_following_returns_only_local_followers(
    pool: PgPool,
) -> sqlx::Result<()> {
    // local actor が remote を follow しているとき、list_local_following で
    // 拾えること。逆向き (remote → local) は拾わない。
    let local = repo::actor::insert(&pool, sample_local_actor("local-f")).await?;
    let mut remote_new = sample_local_actor("remote-f");
    remote_new.is_local = false;
    remote_new.ap_id = "https://remote.test/users/bob".into();
    remote_new.preferred_username = "bob".into();
    remote_new.host = "remote.test".into();
    remote_new.private_key_pem = None;
    let remote = repo::actor::insert(&pool, remote_new).await?;

    // local → remote follow (accepted)。
    let f1 = repo::follow::insert_pending(
        &pool,
        "https://example.test/follows/local-to-remote",
        local.id,
        remote.id,
    )
    .await?;
    repo::follow::set_state(&pool, f1.id, FollowState::Accepted).await?;

    // 逆 (remote → local) も accepted で作っておく。これは list_local_following
    // (followed = remote, local が follower 側) で **拾われない** ことを確認する。
    let f2 = repo::follow::insert_pending(
        &pool,
        "https://example.test/follows/remote-to-local",
        remote.id,
        local.id,
    )
    .await?;
    repo::follow::set_state(&pool, f2.id, FollowState::Accepted).await?;

    let listing = repo::follow::list_local_following(&pool, remote.id).await?;
    assert_eq!(listing.len(), 1);
    assert_eq!(listing[0].0, f1.id);
    assert_eq!(listing[0].1, local.id);

    // remote 側に対して同じ問い合わせ: local が follower なので 1 件出るが、
    // local が followed の側に居る場合は 0 件 (= remote 自身は is_local=false)。
    let none = repo::follow::list_local_following(&pool, local.id).await?;
    assert!(
        none.is_empty(),
        "remote follower should not appear: {none:?}"
    );
    Ok(())
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn emoji_upsert_remote_rejects_empty_host(pool: PgPool) -> sqlx::Result<()> {
    let err = repo::emoji::upsert_remote(
        &pool,
        repo::emoji::NewRemoteEmoji {
            shortcode: "blob".into(),
            ap_id: "https://misskey.io/emojis/blob".into(),
            host: String::new(),
            image_key: Some("emoji/remote/misskey.io/blob.webp".into()),
            media_type: "image/webp".into(),
            last_failed_at: None,
        },
    )
    .await
    .unwrap_err();
    assert!(format!("{err}").contains("host"));
    Ok(())
}
