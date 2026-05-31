//! M8 PR2 統合テスト: `POST /api/v1/reactions` / `DELETE /api/v1/reactions/{id}`。
//!
//! `local_api_pg.rs` のパターンに揃え、本物の token + 本物の Note 行に対して
//! ローカル user がリアクションを作成・削除する経路を検証する。connect-string は
//! `from_pool` 経由なので versitygw / media-proxy には到達しない。

#![forbid(unsafe_code)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use sakurasato_core::model::Visibility;
use sakurasato_core::repo;
use sqlx::PgPool;
use tower::ServiceExt;

mod common {
    use ed25519_dalek::SigningKey;
    use ed25519_dalek::pkcs8::EncodePublicKey;
    use ed25519_dalek::pkcs8::spki::der::pem::LineEnding;
    use rsa::rand_core::OsRng;
    use sakurasato_core::repo::actor::NewActor;

    pub(super) fn sample_local_actor(username: &str, host: &str) -> NewActor {
        let ap_id = format!("https://{host}/users/{username}");
        NewActor {
            ap_id: ap_id.clone(),
            preferred_username: username.into(),
            host: host.into(),
            display_name: Some("Alice".into()),
            summary: None,
            icon_url: None,
            image_url: None,
            inbox_url: format!("{ap_id}/inbox"),
            shared_inbox_url: Some(format!("https://{host}/inbox")),
            outbox_url: Some(format!("{ap_id}/outbox")),
            followers_url: Some(format!("{ap_id}/followers")),
            following_url: Some(format!("{ap_id}/following")),
            public_key_id: format!("{ap_id}#main-key"),
            public_key_pem: "-----BEGIN PUBLIC KEY-----\nMOCK\n-----END PUBLIC KEY-----".into(),
            private_key_pem: Some(
                "-----BEGIN PRIVATE KEY-----\nMOCK\n-----END PRIVATE KEY-----".into(),
            ),
            ed25519_public_key_id: Some(format!("{ap_id}#ed25519-key")),
            ed25519_public_key_pem: Some(sample_ed25519_public_pem()),
            ed25519_private_key_pem: Some(
                "-----BEGIN PRIVATE KEY-----\nMOCK-ED\n-----END PRIVATE KEY-----".into(),
            ),
            also_known_as: vec![],
            moved_to_ap_id: None,
            is_local: true,
            actor_type: "Person".into(),
            manually_approves_followers: false,
        }
    }

    fn sample_ed25519_public_pem() -> String {
        let signing = SigningKey::generate(&mut OsRng);
        signing
            .verifying_key()
            .to_public_key_pem(LineEnding::LF)
            .unwrap()
    }
}

fn make_config(host: &str) -> sakurasato_core::Config {
    sakurasato_core::Config {
        server: sakurasato_core::config::ServerConfig {
            host: host.into(),
            bind: "127.0.0.1:0".into(),
            local_api_socket: "/tmp/sakurasato.sock".into(),
            public_listen: None,
            local_api_listen: None,
            user: "alice".into(),
        },
        database: sakurasato_core::config::DatabaseConfig {
            url: "unused-by-tests".into(),
            password_file: None,
        },
        storage: sakurasato_core::config::StorageConfig {
            endpoint: "http://localhost".into(),
            bucket: "b".into(),
            region: "us-east-1".into(),
            access_key_id: "k".into(),
            secret_access_key: "s".into(),
            secret_access_key_file: None,
        },
        media_proxy: sakurasato_core::config::MediaProxyConfig {
            socket: "/tmp/x".into(),
            max_bytes: 1024,
            max_pixels: 1024,
        },
    }
}

async fn issue_token(pool: &PgPool, name: &str) -> String {
    let raw = sakurasato_server::token::generate_raw();
    let hash = sakurasato_server::token::hash(&raw);
    repo::api_token::insert(
        pool,
        sakurasato_core::repo::api_token::NewApiToken {
            name: name.into(),
            token_hash: hash,
        },
    )
    .await
    .unwrap();
    raw
}

async fn seed_note(pool: &PgPool, actor_id: i64, host: &str) -> i64 {
    let ap_id = format!("https://{host}/notes/1");
    let inserted = repo::note::insert(
        pool,
        repo::note::NewNote {
            ap_id: ap_id.clone(),
            actor_id,
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
            url: Some(ap_id),
            published_at: chrono::Utc::now(),
        },
    )
    .await
    .unwrap();
    inserted.id
}

async fn read_json(resp: axum::response::Response) -> serde_json::Value {
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap()
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn create_reaction_unicode_inserts_row(pool: PgPool) {
    let actor = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let note_id = seed_note(&pool, actor.id, "example.test").await;
    let raw = issue_token(&pool, "tui").await;
    let state =
        sakurasato_server::state::AppState::from_pool(pool.clone(), make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let body = serde_json::json!({"note_id": note_id, "content": "👍"});
    let resp = app
        .oneshot(
            Request::post("/api/v1/reactions")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = read_json(resp).await;
    assert_eq!(json["content"], "👍");
    assert_eq!(json["note_id"], note_id);
    assert!(json["emoji_id"].is_null());
    let ap_id = json["ap_id"].as_str().unwrap();
    assert!(ap_id.starts_with("https://example.test/users/alice/activities/reaction-"));

    // DB に行があり、エンキューは 0 件 (followers 0)。
    let row = repo::reaction::get_by_ap_id(&pool, ap_id).await.unwrap();
    assert!(row.is_some());
    assert_eq!(json["queued_deliveries"], 0);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn create_reaction_local_shortcode_resolves_emoji(pool: PgPool) {
    let actor = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let note_id = seed_note(&pool, actor.id, "example.test").await;
    let raw = issue_token(&pool, "tui").await;

    let emoji = repo::emoji::upsert_local(
        &pool,
        repo::emoji::NewLocalEmoji {
            shortcode: "blob_party".into(),
            category: None,
            aliases: vec![],
            image_key: "emoji/local/blob_party.webp".into(),
            media_type: "image/webp".into(),
        },
    )
    .await
    .unwrap();

    let state =
        sakurasato_server::state::AppState::from_pool(pool.clone(), make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let body = serde_json::json!({"note_id": note_id, "content": ":blob_party:"});
    let resp = app
        .oneshot(
            Request::post("/api/v1/reactions")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = read_json(resp).await;
    assert_eq!(json["content"], ":blob_party:");
    assert_eq!(json["emoji_id"], emoji.id);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn create_reaction_unknown_local_shortcode_returns_404(pool: PgPool) {
    let actor = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let note_id = seed_note(&pool, actor.id, "example.test").await;
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let body = serde_json::json!({"note_id": note_id, "content": ":not_imported:"});
    let resp = app
        .oneshot(
            Request::post("/api/v1/reactions")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn create_reaction_remote_shortcode_returns_400(pool: PgPool) {
    let actor = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let note_id = seed_note(&pool, actor.id, "example.test").await;
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let body = serde_json::json!({"note_id": note_id, "content": ":blob@misskey.io:"});
    let resp = app
        .oneshot(
            Request::post("/api/v1/reactions")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn create_reaction_duplicate_returns_existing(pool: PgPool) {
    let actor = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let note_id = seed_note(&pool, actor.id, "example.test").await;
    let raw = issue_token(&pool, "tui").await;
    let state =
        sakurasato_server::state::AppState::from_pool(pool.clone(), make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let body = serde_json::json!({"note_id": note_id, "content": "👍"});
    let first = app
        .clone()
        .oneshot(
            Request::post("/api/v1/reactions")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let first_json = read_json(first).await;
    let first_id = first_json["id"].as_i64().unwrap();

    let second = app
        .oneshot(
            Request::post("/api/v1/reactions")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::CREATED);
    let second_json = read_json(second).await;
    assert_eq!(second_json["id"], first_id, "same row returned");
    // 重複は配送し直さない。
    assert_eq!(second_json["queued_deliveries"], 0);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn delete_reaction_undoes_and_removes(pool: PgPool) {
    let actor = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let note_id = seed_note(&pool, actor.id, "example.test").await;
    let raw = issue_token(&pool, "tui").await;
    let state =
        sakurasato_server::state::AppState::from_pool(pool.clone(), make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    // まず作る。
    let body = serde_json::json!({"note_id": note_id, "content": "👍"});
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/v1/reactions")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let json = read_json(resp).await;
    let id = json["id"].as_i64().unwrap();
    let ap_id = json["ap_id"].as_str().unwrap().to_string();

    // DELETE。
    let resp = app
        .oneshot(
            Request::delete(format!("/api/v1/reactions/{id}"))
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 行が消えている。
    let gone = repo::reaction::get_by_ap_id(&pool, &ap_id).await.unwrap();
    assert!(gone.is_none(), "reaction row must be deleted");
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn delete_reaction_not_found_returns_404(pool: PgPool) {
    repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = app
        .oneshot(
            Request::delete("/api/v1/reactions/99999")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn create_reaction_rejects_empty_content(pool: PgPool) {
    let actor = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let note_id = seed_note(&pool, actor.id, "example.test").await;
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let body = serde_json::json!({"note_id": note_id, "content": ""});
    let resp = app
        .oneshot(
            Request::post("/api/v1/reactions")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn create_reaction_unknown_note_returns_404(pool: PgPool) {
    repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let body = serde_json::json!({"note_id": 99999, "content": "👍"});
    let resp = app
        .oneshot(
            Request::post("/api/v1/reactions")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ────────────────────────────────────────────────────────────────────────
// M9 着手前 reaction outbound 改善 (#1 note 作者 inbox / #2 Undo inline /
// #3 _misskey_reaction 併載) のリグレッション。
// ────────────────────────────────────────────────────────────────────────

/// 単体テスト用の remote actor を 1 体作る。`shared_inbox_url` を持つ
/// (= 配送圧縮対象)。
fn sample_remote_actor(username: &str, host: &str) -> repo::actor::NewActor {
    let ap_id = format!("https://{host}/users/{username}");
    repo::actor::NewActor {
        ap_id: ap_id.clone(),
        preferred_username: username.into(),
        host: host.into(),
        display_name: Some(username.into()),
        summary: None,
        icon_url: None,
        image_url: None,
        inbox_url: format!("{ap_id}/inbox"),
        shared_inbox_url: Some(format!("https://{host}/inbox")),
        outbox_url: Some(format!("{ap_id}/outbox")),
        followers_url: Some(format!("{ap_id}/followers")),
        following_url: Some(format!("{ap_id}/following")),
        public_key_id: format!("{ap_id}#main-key"),
        public_key_pem: "-----BEGIN PUBLIC KEY-----\nMOCK\n-----END PUBLIC KEY-----".into(),
        private_key_pem: None,
        ed25519_public_key_id: None,
        ed25519_public_key_pem: None,
        ed25519_private_key_pem: None,
        also_known_as: vec![],
        moved_to_ap_id: None,
        is_local: false,
        actor_type: "Person".into(),
        manually_approves_followers: false,
    }
}

async fn seed_remote_note(pool: &PgPool, actor_id: i64, ap_id: &str) -> i64 {
    let inserted = repo::note::insert(
        pool,
        repo::note::NewNote {
            ap_id: ap_id.into(),
            actor_id,
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
            is_local: false,
            url: Some(ap_id.into()),
            published_at: chrono::Utc::now(),
        },
    )
    .await
    .unwrap();
    inserted.id
}

#[allow(clippy::similar_names)] // follower_id / followed_id は AP 用語
async fn accepted_follow(pool: &PgPool, follower_id: i64, followed_id: i64) {
    let row = repo::follow::insert_pending(
        pool,
        &format!("https://example.test/follows/{follower_id}-{followed_id}"),
        follower_id,
        followed_id,
    )
    .await
    .unwrap();
    repo::follow::set_state(pool, row.id, sakurasato_core::model::FollowState::Accepted)
        .await
        .unwrap();
}

async fn list_delivery_queue(pool: &PgPool) -> Vec<(String, serde_json::Value)> {
    sqlx::query!(r#"SELECT inbox_url, activity FROM delivery_queue ORDER BY id"#)
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|r| (r.inbox_url, r.activity))
        .collect()
}

/// `_misskey_reaction` 併載 + `tag` の Misskey 互換 shape を検証する。
/// 旧 Misskey は `EmojiReact` ではなく `_misskey_reaction` だけ読むので、
/// 同じ値を併載しないと旧系列で見えなくなる。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn create_emoji_reaction_includes_misskey_reaction_and_tag(pool: PgPool) {
    let alice = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let follower = repo::actor::insert(&pool, sample_remote_actor("bob", "remote.test"))
        .await
        .unwrap();
    accepted_follow(&pool, follower.id, alice.id).await;
    let note_id = seed_note(&pool, alice.id, "example.test").await;
    repo::emoji::upsert_local(
        &pool,
        repo::emoji::NewLocalEmoji {
            shortcode: "blob_party".into(),
            category: None,
            aliases: vec![],
            image_key: "emoji/local/blob_party.webp".into(),
            media_type: "image/webp".into(),
        },
    )
    .await
    .unwrap();
    let raw = issue_token(&pool, "tui").await;
    let state =
        sakurasato_server::state::AppState::from_pool(pool.clone(), make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let body = serde_json::json!({"note_id": note_id, "content": ":blob_party:"});
    let resp = app
        .oneshot(
            Request::post("/api/v1/reactions")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let queued = list_delivery_queue(&pool).await;
    assert_eq!(queued.len(), 1, "1 delivery (remote follower)");
    let (inbox, activity) = &queued[0];
    assert_eq!(inbox, "https://remote.test/inbox");
    assert_eq!(activity["type"], "EmojiReact");
    assert_eq!(activity["content"], ":blob_party:");
    assert_eq!(activity["_misskey_reaction"], ":blob_party:");
    let tag = activity["tag"].as_array().expect("tag is array");
    assert_eq!(tag.len(), 1);
    assert_eq!(tag[0]["type"], "Emoji");
    assert_eq!(tag[0]["name"], ":blob_party:");
    assert_eq!(tag[0]["icon"]["mediaType"], "image/webp");
}

/// Unicode (Like) 経路では `_misskey_reaction` も `tag` も付かないことを検証。
/// Mastodon に届くので余計な拡張フィールドを混ぜない。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn create_unicode_reaction_omits_misskey_extensions(pool: PgPool) {
    let alice = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let follower = repo::actor::insert(&pool, sample_remote_actor("bob", "remote.test"))
        .await
        .unwrap();
    accepted_follow(&pool, follower.id, alice.id).await;
    let note_id = seed_note(&pool, alice.id, "example.test").await;
    let raw = issue_token(&pool, "tui").await;
    let state =
        sakurasato_server::state::AppState::from_pool(pool.clone(), make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let body = serde_json::json!({"note_id": note_id, "content": "👍"});
    let resp = app
        .oneshot(
            Request::post("/api/v1/reactions")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let queued = list_delivery_queue(&pool).await;
    assert_eq!(queued.len(), 1);
    let activity = &queued[0].1;
    assert_eq!(activity["type"], "Like");
    assert_eq!(activity["content"], "👍");
    assert!(activity.get("_misskey_reaction").is_none());
    assert!(activity.get("tag").is_none());
}

/// remote note 上の reaction を DELETE すると、Undo.object は **元 Activity を
/// inline 埋め込み**で乗り、配送先には **note 作者の (shared) inbox** が含まれる
/// (= フォロワー集合に居なくても相手に届く)。M8 PR2 の URI 参照のみ実装からの
/// 改善。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn delete_remote_note_reaction_inlines_undo_object_and_targets_author(pool: PgPool) {
    let alice = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let charlie = repo::actor::insert(&pool, sample_remote_actor("charlie", "remote.test"))
        .await
        .unwrap();
    let remote_note_ap = "https://remote.test/users/charlie/notes/42";
    let note_id = seed_remote_note(&pool, charlie.id, remote_note_ap).await;

    // ローカル user が「他人の remote note」にリアクションを残した状態を直接
    // 生成する (POST 経路は remote note を 404 で拒否するので DB に直挿入)。
    let reaction_ap = "https://example.test/users/alice/activities/reaction-100";
    let row = repo::reaction::insert_or_get(&pool, reaction_ap, note_id, alice.id, "👍", None)
        .await
        .unwrap();

    let raw = issue_token(&pool, "tui").await;
    let state =
        sakurasato_server::state::AppState::from_pool(pool.clone(), make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = app
        .oneshot(
            Request::delete(format!("/api/v1/reactions/{}", row.id))
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let queued = list_delivery_queue(&pool).await;
    assert_eq!(
        queued.len(),
        1,
        "note 作者 (remote) の inbox に 1 件 (followers 0)"
    );
    let (inbox, activity) = &queued[0];
    assert_eq!(
        inbox, "https://remote.test/inbox",
        "shared_inbox_url が優先される"
    );
    assert_eq!(activity["type"], "Undo");
    let object = &activity["object"];
    assert!(
        object.is_object(),
        "Undo.object は inline JSON でなければならない"
    );
    assert_eq!(object["id"], reaction_ap);
    assert_eq!(object["type"], "Like");
    assert_eq!(object["object"], remote_note_ap);
    assert_eq!(object["content"], "👍");
}

/// `delete_reaction_to_remote_note_via_emoji_includes_misskey_reaction_in_undo`
/// — Undo の inline object でも `_misskey_reaction` / `tag` が保たれる。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn delete_emoji_reaction_undo_preserves_misskey_reaction(pool: PgPool) {
    let alice = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let charlie = repo::actor::insert(&pool, sample_remote_actor("charlie", "remote.test"))
        .await
        .unwrap();
    let remote_note_ap = "https://remote.test/users/charlie/notes/77";
    let note_id = seed_remote_note(&pool, charlie.id, remote_note_ap).await;
    let emoji = repo::emoji::upsert_local(
        &pool,
        repo::emoji::NewLocalEmoji {
            shortcode: "blob_party".into(),
            category: None,
            aliases: vec![],
            image_key: "emoji/local/blob_party.webp".into(),
            media_type: "image/webp".into(),
        },
    )
    .await
    .unwrap();
    let row = repo::reaction::insert_or_get(
        &pool,
        "https://example.test/users/alice/activities/reaction-200",
        note_id,
        alice.id,
        ":blob_party:",
        Some(emoji.id),
    )
    .await
    .unwrap();

    let raw = issue_token(&pool, "tui").await;
    let state =
        sakurasato_server::state::AppState::from_pool(pool.clone(), make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);
    let resp = app
        .oneshot(
            Request::delete(format!("/api/v1/reactions/{}", row.id))
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let queued = list_delivery_queue(&pool).await;
    assert_eq!(queued.len(), 1);
    let object = &queued[0].1["object"];
    assert_eq!(object["type"], "EmojiReact");
    assert_eq!(object["content"], ":blob_party:");
    assert_eq!(object["_misskey_reaction"], ":blob_party:");
    assert_eq!(object["tag"][0]["name"], ":blob_party:");
}
