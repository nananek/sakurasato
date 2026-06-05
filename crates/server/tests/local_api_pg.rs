//! M4 PR1 統合テスト: ローカル API (Unix socket) のルータ + 認証 + `/whoami`。
//!
//! 実 Postgres 上で `#[sqlx::test]` がパー DB を切り、`router::oneshot` で
//! ハンドラを叩く。`tower::ServiceExt::oneshot` を使うので Unix socket は
//! 立てない (PR1 では `axum::Router` の挙動を検証するのが目的)。

#![forbid(unsafe_code)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
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
            summary: Some("hello".into()),
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

    pub(super) fn sample_ed25519_public_pem() -> String {
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
            info: sakurasato_core::config::ServerInfo::default(),
            auto_approve_followers_for_followees: false,
            max_note_text_length: 3000,
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
            public_base_url: None,
        },
        media_proxy: sakurasato_core::config::MediaProxyConfig {
            socket: "/tmp/x".into(),
            max_bytes: 1024,
            max_pixels: 1024,
        },
        miauth: None,
    }
}

async fn read_json(resp: axum::response::Response) -> serde_json::Value {
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap()
}

/// auth middleware から扱えるトークンを 1 本仕込む。生トークンを返すので
/// `Bearer <raw>` を組み立ててリクエストに付ければ通る。
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

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn whoami_rejects_missing_authorization(pool: PgPool) {
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = app
        .oneshot(Request::get("/api/v1/whoami").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    // WWW-Authenticate ヘッダで client にチャレンジスキームを伝える。
    let auth = resp.headers().get(header::WWW_AUTHENTICATE).unwrap();
    assert!(auth.to_str().unwrap().starts_with("Bearer"));
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn whoami_rejects_malformed_authorization(pool: PgPool) {
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = app
        .oneshot(
            Request::get("/api/v1/whoami")
                .header(header::AUTHORIZATION, "Basic foo:bar")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn whoami_rejects_unknown_token(pool: PgPool) {
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = app
        .oneshot(
            Request::get("/api/v1/whoami")
                .header(header::AUTHORIZATION, "Bearer not-a-real-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn whoami_returns_actor_with_valid_token(pool: PgPool) {
    repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let raw = issue_token(&pool, "tui-laptop").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = app
        .oneshot(
            Request::get("/api/v1/whoami")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    assert_eq!(json["ap_id"], "https://example.test/users/alice");
    assert_eq!(json["preferred_username"], "alice");
    assert_eq!(json["host"], "example.test");
    assert_eq!(json["display_name"], "Alice");
    assert_eq!(json["inbox"], "https://example.test/users/alice/inbox");
    // 秘密鍵は当然出ない (シリアライザに乗らないし、whoami ハンドラの
    // レスポンス型にもフィールドが無い)。念のため文字列レベルで確認。
    let body = serde_json::to_string(&json).unwrap();
    assert!(!body.contains("private_key"), "private_key leaked: {body}");
    assert!(!body.contains("MOCK"), "raw PEM leaked: {body}");
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn whoami_404_when_local_actor_missing(pool: PgPool) {
    // 認証は通すがアクター未 init の状態。`sakurasato init` 前の挙動を模す。
    let raw = issue_token(&pool, "tui-laptop").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = app
        .oneshot(
            Request::get("/api/v1/whoami")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ============================================================
// M4 PR2 — timeline / POST notes / SSE
// ============================================================

async fn insert_local_note(
    pool: &PgPool,
    actor_id: i64,
    host: &str,
    suffix: &str,
    content: &str,
) -> i64 {
    let ap_id = format!("https://{host}/notes/{suffix}");
    let row = repo::note::insert(
        pool,
        sakurasato_core::repo::note::NewNote {
            ap_id,
            actor_id,
            content: content.into(),
            language: Some("ja".into()),
            in_reply_to_ap_id: None,
            in_reply_to_note_id: None,
            summary: None,
            visibility: sakurasato_core::model::Visibility::Public,
            sensitive: false,
            to_recipients: vec!["https://www.w3.org/ns/activitystreams#Public".into()],
            cc_recipients: vec![],
            attachments: serde_json::json!([]),
            tags: serde_json::json!([]),
            is_local: true,
            url: None,
            published_at: chrono::Utc::now(),
        },
    )
    .await
    .unwrap();
    row.id
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn timeline_home_503_when_local_actor_missing(pool: PgPool) {
    // local actor 未 init の状態。認証通過 → 503 を返す。
    let raw = issue_token(&pool, "tui-laptop").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = app
        .oneshot(
            Request::get("/api/v1/timeline/home")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn timeline_home_returns_local_and_followed_notes(pool: PgPool) {
    let me = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let mut bob = common::sample_local_actor("bob", "remote.test");
    bob.is_local = false;
    bob.private_key_pem = None;
    bob.ed25519_private_key_pem = None;
    let bob = repo::actor::insert(&pool, bob).await.unwrap();
    let mut carol = common::sample_local_actor("carol", "other.test");
    carol.is_local = false;
    carol.private_key_pem = None;
    carol.ed25519_private_key_pem = None;
    let carol = repo::actor::insert(&pool, carol).await.unwrap();

    // alice -> bob は accepted、alice -> carol は pending。carol の投稿は出ない。
    let bob_follow_ap_id = format!("{}/follows/bob-by-alice", me.ap_id);
    let row = repo::follow::upsert_pending(&pool, &bob_follow_ap_id, me.id, bob.id)
        .await
        .unwrap();
    repo::follow::set_state(&pool, row.id, sakurasato_core::model::FollowState::Accepted)
        .await
        .unwrap();
    let carol_follow_ap_id = format!("{}/follows/carol-by-alice", me.ap_id);
    let _ = repo::follow::upsert_pending(&pool, &carol_follow_ap_id, me.id, carol.id)
        .await
        .unwrap();

    let mine = insert_local_note(&pool, me.id, "example.test", "n1", "hi from alice").await;
    let bobs = insert_local_note(&pool, bob.id, "remote.test", "n2", "hi from bob").await;
    let carols_invisible =
        insert_local_note(&pool, carol.id, "other.test", "n3", "carol pending").await;

    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = app
        .oneshot(
            Request::get("/api/v1/timeline/home")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    let notes = json["notes"].as_array().unwrap();
    let ids: Vec<i64> = notes.iter().map(|n| n["id"].as_i64().unwrap()).collect();
    assert!(ids.contains(&mine), "alice's own note must appear: {ids:?}");
    assert!(ids.contains(&bobs), "bob's note must appear: {ids:?}");
    assert!(
        ids.iter().all(|id| *id != carols_invisible),
        "carol (pending follow) must not appear: {ids:?}",
    );
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn timeline_home_includes_reaction_counts(pool: PgPool) {
    // M8 PR3: timeline 応答に `reactions: [{content, count, emoji_image_url?, ...}]`
    // が含まれることを検証する。
    let me = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let mut bob = common::sample_local_actor("bob", "remote.test");
    bob.is_local = false;
    bob.private_key_pem = None;
    bob.ed25519_private_key_pem = None;
    let bob = repo::actor::insert(&pool, bob).await.unwrap();

    // Note は alice の local 投稿。
    let note_id = insert_local_note(&pool, me.id, "example.test", "reactnote", "hello").await;

    // bob が 👍 を 1 回、alice 自身が :blob: を 1 回 (= ローカル emoji 学習済み)。
    repo::reaction::insert(
        &pool,
        "https://remote.test/users/bob/r/1",
        note_id,
        bob.id,
        "👍",
        None,
    )
    .await
    .unwrap();
    let emoji = repo::emoji::upsert_local(
        &pool,
        repo::emoji::NewLocalEmoji {
            shortcode: "blob".into(),
            category: None,
            aliases: vec![],
            image_key: "emoji/local/blob.webp".into(),
            media_type: "image/webp".into(),
        },
    )
    .await
    .unwrap();
    repo::reaction::insert(
        &pool,
        "https://example.test/users/alice/r/1",
        note_id,
        me.id,
        ":blob:",
        Some(emoji.id),
    )
    .await
    .unwrap();

    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = app
        .oneshot(
            Request::get("/api/v1/timeline/home")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    let notes = json["notes"].as_array().unwrap();
    let n = notes
        .iter()
        .find(|n| n["id"].as_i64() == Some(note_id))
        .expect("note must be present");
    let reactions = n["reactions"].as_array().expect("reactions field present");
    assert_eq!(reactions.len(), 2, "two distinct contents: {reactions:?}");
    // 並びは MIN(created_at)。bob の Like → alice の :blob: の順。
    assert_eq!(reactions[0]["content"], "👍");
    assert_eq!(reactions[0]["count"], 1);
    assert!(reactions[0]["emoji_image_url"].is_null());
    assert_eq!(reactions[1]["content"], ":blob:");
    assert_eq!(reactions[1]["count"], 1);
    // local emoji → image_url が `/media/emoji/local/blob.webp` に展開される。
    assert_eq!(
        reactions[1]["emoji_image_url"]
            .as_str()
            .expect("emoji_image_url for local"),
        "https://example.test/media/emoji/local/blob.webp",
    );
    assert_eq!(reactions[1]["emoji_is_local"], serde_json::json!(true));
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn create_note_persists_and_enqueues(pool: PgPool) {
    let me = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    // 1 人だけ follower (= remote actor で shared_inbox 持ち) を仕込む。
    let mut bob = common::sample_local_actor("bob", "remote.test");
    bob.is_local = false;
    bob.private_key_pem = None;
    bob.ed25519_private_key_pem = None;
    bob.shared_inbox_url = Some("https://remote.test/inbox".into());
    let bob = repo::actor::insert(&pool, bob).await.unwrap();
    // bob が alice を follow している → 配送先は bob の shared_inbox。
    let f_ap_id = "https://remote.test/follows/alice-by-bob".to_string();
    let row = repo::follow::upsert_pending(&pool, &f_ap_id, bob.id, me.id)
        .await
        .unwrap();
    repo::follow::set_state(&pool, row.id, sakurasato_core::model::FollowState::Accepted)
        .await
        .unwrap();

    let raw = issue_token(&pool, "tui").await;
    let state =
        sakurasato_server::state::AppState::from_pool(pool.clone(), make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let body = serde_json::json!({
        "content": "hello world",
        "visibility": "public",
        "language": "en",
    });
    let resp = app
        .oneshot(
            Request::post("/api/v1/notes")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let loc = resp.headers().get(header::LOCATION).unwrap();
    assert!(loc.to_str().unwrap().starts_with("/notes/"));
    let json = read_json(resp).await;
    assert_eq!(json["content"], "hello world");
    assert_eq!(json["visibility"], "public");
    assert_eq!(json["queued_deliveries"], 1);
    let id = json["id"].as_i64().unwrap();
    // ap_id が canonical URL に書き直されていること。
    assert_eq!(
        json["ap_id"],
        serde_json::Value::String(format!("https://example.test/notes/{id}"))
    );

    // delivery_queue に 1 行積まれ、宛先が bob の shared_inbox であること。
    let inbox_count: i64 = sqlx::query_scalar!(
        "SELECT COUNT(*) AS \"c!\" FROM delivery_queue WHERE inbox_url = $1",
        "https://remote.test/inbox",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(inbox_count, 1);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn create_note_rejects_empty_content(pool: PgPool) {
    repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = app
        .oneshot(
            Request::post("/api/v1/notes")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"content": "   "}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// **#65**: direct visibility は content に `@user@host` mention が無いと
/// 配送先が無いので 400。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn create_note_rejects_direct_without_mention(pool: PgPool) {
    repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let body = serde_json::json!({"content": "no mention here", "visibility": "direct"});
    let resp = app
        .oneshot(
            Request::post("/api/v1/notes")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// **#65**: direct DM が mention された remote actor の inbox にだけ
/// `delivery_queue` 行を積み、followers (= 全く別の remote actor) には
/// 積まないこと。activity の `to` に mention 先 URI が乗り、`cc` は空。
/// `tag` 配列に `Mention` エントリが入る。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn create_note_direct_delivers_only_to_mentioned_inbox(pool: PgPool) {
    let me = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    // Bob: mention 先 (= seed しておく)。
    let mut bob = common::sample_local_actor("bob", "remote.test");
    bob.is_local = false;
    bob.private_key_pem = None;
    bob.ed25519_private_key_pem = None;
    bob.shared_inbox_url = Some("https://remote.test/inbox".into());
    let bob = repo::actor::insert(&pool, bob).await.unwrap();

    // Carol: follower (mention されない別 remote actor)。direct DM は
    // ここには配送されてはならない。
    let mut carol = common::sample_local_actor("carol", "other.test");
    carol.is_local = false;
    carol.private_key_pem = None;
    carol.ed25519_private_key_pem = None;
    carol.shared_inbox_url = Some("https://other.test/inbox".into());
    let carol = repo::actor::insert(&pool, carol).await.unwrap();
    let f_ap_id = "https://other.test/follows/alice-by-carol".to_string();
    let row = repo::follow::upsert_pending(&pool, &f_ap_id, carol.id, me.id)
        .await
        .unwrap();
    repo::follow::set_state(&pool, row.id, sakurasato_core::model::FollowState::Accepted)
        .await
        .unwrap();

    let raw = issue_token(&pool, "tui").await;
    let state =
        sakurasato_server::state::AppState::from_pool(pool.clone(), make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let body = serde_json::json!({
        "content": "@bob@remote.test psst",
        "visibility": "direct",
    });
    let resp = app
        .oneshot(
            Request::post("/api/v1/notes")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = read_json(resp).await;
    assert_eq!(json["visibility"], "direct");
    // mention 先 1 件のみ。Carol (= follower) には行かない。
    assert_eq!(json["queued_deliveries"], 1);

    // delivery_queue: bob の inbox にだけ 1 行ある。carol の inbox 行は無い。
    let bob_count: i64 = sqlx::query_scalar!(
        "SELECT COUNT(*) AS \"c!\" FROM delivery_queue WHERE inbox_url = $1",
        "https://remote.test/inbox",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(bob_count, 1);
    let carol_count: i64 = sqlx::query_scalar!(
        "SELECT COUNT(*) AS \"c!\" FROM delivery_queue WHERE inbox_url = $1",
        "https://other.test/inbox",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(carol_count, 0);

    // activity body: object.to が bob の URI、cc が空、tag に Mention が入る。
    let row = sqlx::query!(r#"SELECT activity FROM delivery_queue LIMIT 1"#)
        .fetch_one(&pool)
        .await
        .unwrap();
    let to: Vec<&str> = row.activity["object"]["to"]
        .as_array()
        .expect("object.to array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(to, vec![bob.ap_id.as_str()], "direct.to should be [bob]");
    let cc = row.activity["object"]["cc"]
        .as_array()
        .expect("object.cc array");
    assert!(cc.is_empty(), "direct.cc should be empty, got {cc:?}");
    let tags = row.activity["object"]["tag"]
        .as_array()
        .expect("object.tag array");
    assert_eq!(tags.len(), 1, "expected 1 Mention tag, got {tags:?}");
    assert_eq!(tags[0]["type"], "Mention");
    assert_eq!(tags[0]["href"], bob.ap_id);
    assert_eq!(tags[0]["name"], "@bob@remote.test");

    // **PR #78 review F-5**: outer Create envelope の to/cc も同値で乗ること
    // (= 将来のリファクタで object 側と乖離しても捕まえる)。
    let outer_to: Vec<&str> = row.activity["to"]
        .as_array()
        .expect("Create.to array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(outer_to, vec![bob.ap_id.as_str()], "Create.to == object.to");
    let outer_cc = row.activity["cc"].as_array().expect("Create.cc array");
    assert!(
        outer_cc.is_empty(),
        "Create.cc should be empty, got {outer_cc:?}",
    );

    // DB の note 行にも tag が永続化されていること (= timeline 等で再利用可能)。
    let note_id = json["id"].as_i64().unwrap();
    let note_tags: serde_json::Value =
        sqlx::query_scalar!(r#"SELECT tags FROM note WHERE id = $1"#, note_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let arr = note_tags.as_array().expect("tags is array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["type"], "Mention");
}

/// **#98**: direct visibility で `in_reply_to_ap_id` だけ指定 (= mention 無し)
/// の場合、activity の `to` は **親 author の actor URI 1 件のみ**、`cc` は
/// 空でなければならない。
///
/// 連合テストで Mastodon 側が `visibility=private` (followers-only) と
/// 認識する事故が報告されていた。Mastodon の `StatusParser#visibility` は
/// `audience_to.include?(@account.followers_url)` で `:private` 判定するので、
/// `to` に followers URL が混入していると private 扱いされる。本テストで
/// followers URL や Public URI が混入しないことを assert する。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn create_note_direct_reply_to_remote_actor_has_only_parent_in_to(pool: PgPool) {
    let me = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    // Bob: 返信先の remote actor。事前に seed note を 1 つ仕込んでおく。
    let mut bob = common::sample_local_actor("bob", "remote.test");
    bob.is_local = false;
    bob.private_key_pem = None;
    bob.ed25519_private_key_pem = None;
    bob.shared_inbox_url = Some("https://remote.test/inbox".into());
    let bob = repo::actor::insert(&pool, bob).await.unwrap();

    let bob_seed_ap_id = "https://remote.test/notes/seed-1";
    repo::note::insert(
        &pool,
        sakurasato_core::repo::note::NewNote {
            ap_id: bob_seed_ap_id.into(),
            actor_id: bob.id,
            content: "seed".into(),
            language: None,
            in_reply_to_ap_id: None,
            in_reply_to_note_id: None,
            summary: None,
            visibility: sakurasato_core::model::Visibility::Public,
            sensitive: false,
            to_recipients: vec!["https://www.w3.org/ns/activitystreams#Public".into()],
            cc_recipients: vec![],
            attachments: serde_json::json!([]),
            tags: serde_json::json!([]),
            is_local: false,
            url: Some(bob_seed_ap_id.into()),
            published_at: chrono::Utc::now(),
        },
    )
    .await
    .unwrap();

    let raw = issue_token(&pool, "tui").await;
    let state =
        sakurasato_server::state::AppState::from_pool(pool.clone(), make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    // content には `@bob@remote.test` mention を入れない (= reply parent 経路で
    // direct を成立させる scenario)。federation test の direct visibility と同形。
    let body = serde_json::json!({
        "content": "direct reply to bob",
        "visibility": "direct",
        "in_reply_to_ap_id": bob_seed_ap_id,
    });
    let resp = app
        .oneshot(
            Request::post("/api/v1/notes")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = read_json(resp).await;
    assert_eq!(json["visibility"], "direct");
    // Bob (parent author) 1 件のみ。followers は direct なので配送しない。
    assert_eq!(json["queued_deliveries"], 1);

    let row = sqlx::query!(r#"SELECT activity FROM delivery_queue LIMIT 1"#)
        .fetch_one(&pool)
        .await
        .unwrap();
    let me_followers = format!("{}/followers", me.ap_id);
    let public_uri = "https://www.w3.org/ns/activitystreams#Public";

    // object.to は bob URI 1 件のみ。followers URL / Public 混入なし。
    let to: Vec<&str> = row.activity["object"]["to"]
        .as_array()
        .expect("object.to array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(
        to,
        vec![bob.ap_id.as_str()],
        "direct reply: object.to must be exactly [bob], got {to:?}",
    );
    assert!(
        !to.contains(&me_followers.as_str()),
        "direct reply must NOT include {me_followers} in object.to (would be parsed as private by Mastodon)",
    );
    assert!(
        !to.contains(&public_uri),
        "direct reply must NOT include Public URI in object.to",
    );

    let cc = row.activity["object"]["cc"]
        .as_array()
        .expect("object.cc array");
    assert!(
        cc.is_empty(),
        "direct reply: object.cc must be empty, got {cc:?}",
    );

    // outer Create envelope も同値。
    let outer_to: Vec<&str> = row.activity["to"]
        .as_array()
        .expect("Create.to array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(
        outer_to,
        vec![bob.ap_id.as_str()],
        "direct reply: Create.to must equal object.to",
    );
    let outer_cc = row.activity["cc"].as_array().expect("Create.cc array");
    assert!(
        outer_cc.is_empty(),
        "direct reply: Create.cc must be empty, got {outer_cc:?}",
    );

    // **#98**: tag.Mention に親 author (bob) が乗っていること。Mastodon は
    // tag.Mention に無い audience を silent mention として扱い、direct を
    // `:limited` に降格 (API では `private` 表示) する。
    let tags = row.activity["object"]["tag"]
        .as_array()
        .expect("object.tag array");
    assert_eq!(
        tags.len(),
        1,
        "expected 1 Mention tag (parent author), got {tags:?}"
    );
    assert_eq!(tags[0]["type"], "Mention");
    assert_eq!(tags[0]["href"], bob.ap_id);
    assert_eq!(
        tags[0]["name"],
        format!("@{}@{}", bob.preferred_username, bob.host)
    );

    // 永続化された note 行の to_recipients / cc_recipients も同値であること
    // (= permalink の AP JSON も同じ値を返す)。
    let note_row = sqlx::query!(
        r#"SELECT to_recipients, cc_recipients FROM note WHERE actor_id = $1"#,
        me.id,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let stored_to: Vec<&str> = note_row
        .to_recipients
        .as_array()
        .expect("to_recipients array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(stored_to, vec![bob.ap_id.as_str()]);
    let stored_cc = note_row
        .cc_recipients
        .as_array()
        .expect("cc_recipients array");
    assert!(stored_cc.is_empty());
}

/// **#65**: 公開投稿でも `@user@host` mention は `cc` に乗り、mention 先
/// inbox にも `delivery_queue` 行が積まれる (= followers 配送と並列)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn create_note_public_with_mention_delivers_to_both(pool: PgPool) {
    let me = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    // Mention 先 (非フォロワー)。
    let mut bob = common::sample_local_actor("bob", "remote.test");
    bob.is_local = false;
    bob.private_key_pem = None;
    bob.ed25519_private_key_pem = None;
    bob.shared_inbox_url = Some("https://remote.test/inbox".into());
    let bob = repo::actor::insert(&pool, bob).await.unwrap();

    // 別の follower (mention されていない)。
    let mut carol = common::sample_local_actor("carol", "other.test");
    carol.is_local = false;
    carol.private_key_pem = None;
    carol.ed25519_private_key_pem = None;
    carol.shared_inbox_url = Some("https://other.test/inbox".into());
    let carol = repo::actor::insert(&pool, carol).await.unwrap();
    let f_ap_id = "https://other.test/follows/alice-by-carol".to_string();
    let row = repo::follow::upsert_pending(&pool, &f_ap_id, carol.id, me.id)
        .await
        .unwrap();
    repo::follow::set_state(&pool, row.id, sakurasato_core::model::FollowState::Accepted)
        .await
        .unwrap();

    let raw = issue_token(&pool, "tui").await;
    let state =
        sakurasato_server::state::AppState::from_pool(pool.clone(), make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let body = serde_json::json!({
        "content": "ping @bob@remote.test",
        "visibility": "public",
    });
    let resp = app
        .oneshot(
            Request::post("/api/v1/notes")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = read_json(resp).await;
    // bob (mention) + carol (follower) → 2 件。
    assert_eq!(json["queued_deliveries"], 2);

    // 両方の inbox にちょうど 1 行ずつ。
    let bob_count: i64 = sqlx::query_scalar!(
        "SELECT COUNT(*) AS \"c!\" FROM delivery_queue WHERE inbox_url = $1",
        "https://remote.test/inbox",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(bob_count, 1);
    let carol_count: i64 = sqlx::query_scalar!(
        "SELECT COUNT(*) AS \"c!\" FROM delivery_queue WHERE inbox_url = $1",
        "https://other.test/inbox",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(carol_count, 1);

    // activity body: object.cc に bob URI + followers URL が入る。
    let row = sqlx::query!(
        r#"SELECT activity FROM delivery_queue WHERE inbox_url = $1"#,
        "https://remote.test/inbox",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let cc: Vec<&str> = row.activity["object"]["cc"]
        .as_array()
        .expect("object.cc array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(cc.iter().any(|s| *s == bob.ap_id));
    let tags = row.activity["object"]["tag"]
        .as_array()
        .expect("object.tag");
    assert_eq!(tags.len(), 1);
    assert_eq!(tags[0]["href"], bob.ap_id);
}

/// **#65**: 解決不能な mention は 400 (= seed されていない remote actor、
/// remote fetch 無効 = テスト経路では弾かれる)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn create_note_rejects_unresolvable_mention(pool: PgPool) {
    repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let body = serde_json::json!({
        "content": "hello @nobody@unknown.example",
        "visibility": "public",
    });
    let resp = app
        .oneshot(
            Request::post("/api/v1/notes")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// **#64**: 未フォローの remote actor の note に reply すると、その author の
/// inbox に対しても `delivery_queue` 行が積まれること (= 親 author 配送)。
/// activity body の `object.cc` にも親 author URI が乗ること。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn create_note_reply_enqueues_to_non_follower_parent_author(pool: PgPool) {
    let me = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    // Charlie: remote actor、follower ではない (= follow 関係なし)。
    let mut charlie = common::sample_local_actor("charlie", "remote.test");
    charlie.is_local = false;
    charlie.private_key_pem = None;
    charlie.ed25519_private_key_pem = None;
    charlie.shared_inbox_url = Some("https://remote.test/inbox".into());
    let charlie = repo::actor::insert(&pool, charlie).await.unwrap();

    // Charlie の remote note を仕込む (= 過去に受領済みという想定)。
    let charlie_note_ap_id = "https://remote.test/notes/seed-123";
    repo::note::insert(
        &pool,
        sakurasato_core::repo::note::NewNote {
            ap_id: charlie_note_ap_id.into(),
            actor_id: charlie.id,
            content: "seed".into(),
            language: None,
            in_reply_to_ap_id: None,
            in_reply_to_note_id: None,
            summary: None,
            visibility: sakurasato_core::model::Visibility::Public,
            sensitive: false,
            to_recipients: vec!["https://www.w3.org/ns/activitystreams#Public".into()],
            cc_recipients: vec![],
            attachments: serde_json::json!([]),
            tags: serde_json::json!([]),
            is_local: false,
            url: Some(charlie_note_ap_id.into()),
            published_at: chrono::Utc::now(),
        },
    )
    .await
    .unwrap();

    let raw = issue_token(&pool, "tui").await;
    let state =
        sakurasato_server::state::AppState::from_pool(pool.clone(), make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let body = serde_json::json!({
        "content": "replying to non-follower",
        "visibility": "public",
        "in_reply_to_ap_id": charlie_note_ap_id,
    });
    let resp = app
        .oneshot(
            Request::post("/api/v1/notes")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = read_json(resp).await;
    // 未フォロー相手だが、親 author の inbox に 1 件積まれている。
    assert_eq!(json["queued_deliveries"], 1);

    // delivery_queue の宛先が Charlie の shared_inbox + activity body の
    // `object.cc` に Charlie の URI が乗っていること。
    let row = sqlx::query!(
        r#"SELECT inbox_url, activity FROM delivery_queue WHERE inbox_url = $1"#,
        "https://remote.test/inbox",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.inbox_url, "https://remote.test/inbox");
    let cc = row.activity["object"]["cc"]
        .as_array()
        .expect("object.cc array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect::<Vec<_>>();
    assert!(
        cc.iter().any(|s| *s == charlie.ap_id),
        "expected {} in object.cc; got {cc:?}",
        charlie.ap_id,
    );
    // note 行も in_reply_to_note_id が立っていること (pre-resolve 経路の確認)。
    let linked = sqlx::query!(
        r#"SELECT in_reply_to_note_id FROM note WHERE actor_id = $1 ORDER BY id DESC LIMIT 1"#,
        me.id,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        linked.in_reply_to_note_id.is_some(),
        "in_reply_to_note_id should be linked",
    );
}

/// **#64**: 親 author が既に follower 集合に居る場合、`delivery_queue` 行を
/// 二重に作らない (`shared_inbox` の重複排除)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn create_note_reply_dedupes_when_parent_author_is_follower(pool: PgPool) {
    let me = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let mut bob = common::sample_local_actor("bob", "remote.test");
    bob.is_local = false;
    bob.private_key_pem = None;
    bob.ed25519_private_key_pem = None;
    bob.shared_inbox_url = Some("https://remote.test/inbox".into());
    let bob = repo::actor::insert(&pool, bob).await.unwrap();
    // Bob は alice の follower (= accepted)。
    let f_ap_id = "https://remote.test/follows/alice-by-bob".to_string();
    let row = repo::follow::upsert_pending(&pool, &f_ap_id, bob.id, me.id)
        .await
        .unwrap();
    repo::follow::set_state(&pool, row.id, sakurasato_core::model::FollowState::Accepted)
        .await
        .unwrap();

    // Bob の remote note を仕込む。
    let bob_note_ap_id = "https://remote.test/notes/from-bob-1";
    repo::note::insert(
        &pool,
        sakurasato_core::repo::note::NewNote {
            ap_id: bob_note_ap_id.into(),
            actor_id: bob.id,
            content: "hi".into(),
            language: None,
            in_reply_to_ap_id: None,
            in_reply_to_note_id: None,
            summary: None,
            visibility: sakurasato_core::model::Visibility::Public,
            sensitive: false,
            to_recipients: vec!["https://www.w3.org/ns/activitystreams#Public".into()],
            cc_recipients: vec![],
            attachments: serde_json::json!([]),
            tags: serde_json::json!([]),
            is_local: false,
            url: Some(bob_note_ap_id.into()),
            published_at: chrono::Utc::now(),
        },
    )
    .await
    .unwrap();

    let raw = issue_token(&pool, "tui").await;
    let state =
        sakurasato_server::state::AppState::from_pool(pool.clone(), make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let body = serde_json::json!({
        "content": "reply",
        "visibility": "public",
        "in_reply_to_ap_id": bob_note_ap_id,
    });
    let resp = app
        .oneshot(
            Request::post("/api/v1/notes")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = read_json(resp).await;
    // followers loop で 1 件、親 author 経路で重複しないため計 1 件。
    assert_eq!(json["queued_deliveries"], 1);
    let count: i64 = sqlx::query_scalar!(
        "SELECT COUNT(*) AS \"c!\" FROM delivery_queue WHERE inbox_url = $1",
        "https://remote.test/inbox",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
    // cc に Bob の URI が乗っていること (follower でも parent 経路で重複は無い)。
    let row = sqlx::query!(r#"SELECT activity FROM delivery_queue LIMIT 1"#)
        .fetch_one(&pool)
        .await
        .unwrap();
    let cc: Vec<&str> = row.activity["object"]["cc"]
        .as_array()
        .expect("object.cc")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    let parent_count = cc.iter().filter(|s| **s == bob.ap_id).count();
    assert_eq!(
        parent_count, 1,
        "parent author URI must appear exactly once in cc: {cc:?}",
    );
}

/// **#64**: 自己 reply (= 自分の note への返信) は、自分の inbox を
/// `delivery_queue` に積まない。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn create_note_self_reply_does_not_enqueue_self(pool: PgPool) {
    let me = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    // 自分の note を 1 つ仕込む。
    let _ = insert_local_note(&pool, me.id, "example.test", "self-1", "first").await;
    let my_note_ap_id = "https://example.test/notes/self-1";

    let raw = issue_token(&pool, "tui").await;
    let state =
        sakurasato_server::state::AppState::from_pool(pool.clone(), make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let body = serde_json::json!({
        "content": "self reply",
        "visibility": "public",
        "in_reply_to_ap_id": my_note_ap_id,
    });
    let resp = app
        .oneshot(
            Request::post("/api/v1/notes")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = read_json(resp).await;
    // followers ゼロ、parent は self なので enqueue 件数は 0。
    assert_eq!(json["queued_deliveries"], 0);
    let count: i64 = sqlx::query_scalar!("SELECT COUNT(*) AS \"c!\" FROM delivery_queue",)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn stream_emits_note_created_event_after_post(pool: PgPool) {
    use http_body_util::BodyStream;
    use tokio_stream::StreamExt as _;

    repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    // SSE を開いてから別タスクで POST を撃つ。subscriber が登録された後に
    // publish が走ることを保証するため、まず stream を await して body を
    // 取得→読み込み開始、その後 publisher を spawn する。
    let stream_resp = app
        .clone()
        .oneshot(
            Request::get("/api/v1/stream")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stream_resp.status(), StatusCode::OK);
    let ct = stream_resp.headers().get(header::CONTENT_TYPE).unwrap();
    assert!(
        ct.to_str().unwrap().starts_with("text/event-stream"),
        "SSE content-type: {ct:?}",
    );

    let body = stream_resp.into_body();
    let mut stream = BodyStream::new(body);

    // ここで `stream_resp` は既に解決済み (= `stream::handle` が
    // `Sender::subscribe()` を呼び終えて Response を返した状態)。
    // よって `Receiver` は broadcast channel に登録済みで、
    // この後 publish される event は確実に受信される。POST を
    // spawn せず直列に撃ち、sleep 同期を完全に排除する (PR #33 review #3)。
    let req_body = serde_json::json!({"content": "broadcast me"});
    let post_resp = app
        .oneshot(
            Request::post("/api/v1/notes")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&req_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(post_resp.status(), StatusCode::CREATED);

    // SSE フレームを最大 2 秒間待ち、`note.created` を見つけたら通過。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut buf = String::new();
    let mut found = false;
    while std::time::Instant::now() < deadline {
        let Some(chunk) =
            tokio::time::timeout(std::time::Duration::from_millis(500), stream.next())
                .await
                .ok()
                .flatten()
        else {
            continue;
        };
        let frame = match chunk {
            Ok(f) => f,
            Err(err) => panic!("stream error: {err:?}"),
        };
        // BodyStream::next() yields hyper::body::Frame; we only care about data.
        if let Ok(data) = frame.into_data() {
            buf.push_str(std::str::from_utf8(&data).unwrap_or(""));
            if buf.contains("event: note.created") && buf.contains("broadcast me") {
                found = true;
                break;
            }
        }
    }
    assert!(found, "SSE did not receive note.created event: {buf}");
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn token_revoke_invalidates_existing_token(pool: PgPool) {
    repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let raw = issue_token(&pool, "tui-laptop").await;
    // 直前で発行した token を hash → row 検索 → delete。
    let hash = sakurasato_server::token::hash(&raw);
    let row = repo::api_token::find_by_hash(&pool, &hash)
        .await
        .unwrap()
        .expect("just-issued token must exist");
    assert!(
        repo::api_token::delete_by_id(&pool, row.id).await.unwrap(),
        "revoke must delete a row",
    );

    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = app
        .oneshot(
            Request::get("/api/v1/whoami")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ============================================================
// M13 PR1 (Issue #79) — `/api/v1/actor` + relationship
// ============================================================

fn sample_remote_actor(username: &str, host: &str) -> sakurasato_core::repo::actor::NewActor {
    let mut a = common::sample_local_actor(username, host);
    a.is_local = false;
    // remote actor は秘密鍵を持たない (= 我々の DB にコピーがある場合のみ
    // 公開鍵を保持する想定)。
    a.private_key_pem = None;
    a.ed25519_private_key_pem = None;
    a
}

#[allow(clippy::similar_names)] // follower/followed は AP の用語
async fn insert_follow(
    pool: &PgPool,
    follower_actor_id: i64,
    followed_actor_id: i64,
    state: sakurasato_core::model::FollowState,
) -> i64 {
    let ap_id = format!("https://test/follow/{follower_actor_id}-{followed_actor_id}");
    let row = repo::follow::insert_pending(pool, &ap_id, follower_actor_id, followed_actor_id)
        .await
        .unwrap();
    if state != sakurasato_core::model::FollowState::Pending {
        repo::follow::set_state(pool, row.id, state).await.unwrap();
    }
    row.id
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn actor_lookup_400_when_no_query_param(pool: PgPool) {
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = app
        .oneshot(
            Request::get("/api/v1/actor")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn actor_lookup_by_ap_id_returns_db_hit_without_remote_fetch(pool: PgPool) {
    repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let bob = repo::actor::insert(&pool, sample_remote_actor("bob", "remote.test"))
        .await
        .unwrap();
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = app
        .oneshot(
            Request::get(format!("/api/v1/actor?ap_id={}", bob.ap_id))
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    assert_eq!(json["actor"]["ap_id"], bob.ap_id);
    assert_eq!(json["actor"]["preferred_username"], "bob");
    assert_eq!(json["actor"]["host"], "remote.test");
    // 秘密鍵漏洩防御 (ActorRow `#[serde(skip)]` の確認)。
    let body = serde_json::to_string(&json).unwrap();
    assert!(!body.contains("private_key"), "private_key leaked: {body}");
    // relationship は initial 状態 (フォロー無し)。
    assert_eq!(json["relationship"]["following"], false);
    assert!(json["relationship"]["follow_state"].is_null());
    assert_eq!(json["relationship"]["followed_by"], false);
    // M13 PR4: follow 行が無いケースは follow_id 自体が JSON に乗らない
    // (`skip_serializing_if = "Option::is_none"`) ── TUI は欠落と null を
    // 同等扱いするので欠落でよい。
    assert!(json["relationship"]["follow_id"].is_null());
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn actor_get_by_id_404_when_missing(pool: PgPool) {
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = app
        .oneshot(
            Request::get("/api/v1/actor/99999")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn actor_get_by_id_returns_actor(pool: PgPool) {
    let bob = repo::actor::insert(&pool, sample_remote_actor("bob", "remote.test"))
        .await
        .unwrap();
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = app
        .oneshot(
            Request::get(format!("/api/v1/actor/{}", bob.id))
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    assert_eq!(json["actor"]["id"], bob.id);
    assert_eq!(json["actor"]["ap_id"], bob.ap_id);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn relationship_neutral_for_self(pool: PgPool) {
    let me = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = app
        .oneshot(
            Request::get(format!("/api/v1/actor/{}/relationship", me.id))
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    assert_eq!(json["following"], false);
    assert!(json["follow_state"].is_null());
    assert_eq!(json["followed_by"], false);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn relationship_reflects_follow_states(pool: PgPool) {
    let me = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let bob = repo::actor::insert(&pool, sample_remote_actor("bob", "remote.test"))
        .await
        .unwrap();
    let carol = repo::actor::insert(&pool, sample_remote_actor("carol", "remote.test"))
        .await
        .unwrap();

    // me → bob は accepted (mutual の片方)。
    insert_follow(
        &pool,
        me.id,
        bob.id,
        sakurasato_core::model::FollowState::Accepted,
    )
    .await;
    // bob → me も accepted (mutual)。
    insert_follow(
        &pool,
        bob.id,
        me.id,
        sakurasato_core::model::FollowState::Accepted,
    )
    .await;
    // me → carol は pending (= まだ Accept が返ってきていない)。
    insert_follow(
        &pool,
        me.id,
        carol.id,
        sakurasato_core::model::FollowState::Pending,
    )
    .await;

    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state.clone());

    // mutual case: following=true, followed_by=true。
    let resp = app
        .clone()
        .oneshot(
            Request::get(format!("/api/v1/actor/{}/relationship", bob.id))
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    assert_eq!(json["following"], true, "{json}");
    assert_eq!(json["follow_state"], "accepted", "{json}");
    assert_eq!(json["followed_by"], true, "{json}");
    // M13 PR4: accepted のとき follow_id を露出する (= TUI Profile `f` toggle
    // が DELETE /api/v1/follow/{id} に渡す引数源)。
    assert!(json["follow_id"].is_i64(), "{json}");

    // pending case: following=false, follow_state=pending, followed_by=false。
    let resp = app
        .oneshot(
            Request::get(format!("/api/v1/actor/{}/relationship", carol.id))
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    assert_eq!(json["following"], false, "{json}");
    assert_eq!(json["follow_state"], "pending", "{json}");
    assert_eq!(json["followed_by"], false, "{json}");
    // M13 PR4: pending のときも follow_id は露出 (= unfollow CTA を出して
    // 取り消しできるようにするため)。
    assert!(json["follow_id"].is_i64(), "{json}");
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn relationship_404_when_target_missing(pool: PgPool) {
    repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = app
        .oneshot(
            Request::get("/api/v1/actor/99999/relationship")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn actor_lookup_requires_auth(pool: PgPool) {
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = app
        .oneshot(
            Request::get("/api/v1/actor?ap_id=https://x/users/y")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ============================================================
// M13 PR2 (Issue #79) — `POST /api/v1/follow` + `DELETE /api/v1/follow/{id}`
// ============================================================

/// `POST /api/v1/follow {actor_id}` ── 既に DB に居る remote actor を follow。
/// `delivery_queue` に Follow が 1 行積まれ、`follow.state = pending` になる。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn follow_creates_pending_and_enqueues(pool: PgPool) {
    let me = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let mut bob = sample_remote_actor("bob", "remote.test");
    bob.shared_inbox_url = Some("https://remote.test/inbox".into());
    let bob = repo::actor::insert(&pool, bob).await.unwrap();

    let raw = issue_token(&pool, "tui").await;
    let state =
        sakurasato_server::state::AppState::from_pool(pool.clone(), make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let body = serde_json::json!({"actor_id": bob.id});
    let resp = app
        .oneshot(
            Request::post("/api/v1/follow")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    assert_eq!(json["target_actor_id"], bob.id);
    assert_eq!(json["state"], "pending");
    assert_eq!(json["already_accepted"], false);
    assert!(json["delivery_queue_id"].is_i64());
    assert_eq!(json["inbox_url"], "https://remote.test/inbox");

    // follow 行が pending で存在。
    let follow_row = repo::follow::get_by_pair(&pool, me.id, bob.id)
        .await
        .unwrap()
        .expect("follow row inserted");
    assert_eq!(follow_row.state, "pending");

    // delivery_queue に Follow が 1 行。
    let count: i64 = sqlx::query_scalar!(
        "SELECT COUNT(*) AS \"c!\" FROM delivery_queue WHERE inbox_url = $1",
        "https://remote.test/inbox",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
}

/// 既存 `accepted` の follow を再 POST → idempotent (200 + `already_accepted=true`)、
/// `delivery_queue` には 1 行も増えない (= 余分な Follow を再送しない)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn follow_idempotent_when_already_accepted(pool: PgPool) {
    let me = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let bob = repo::actor::insert(&pool, sample_remote_actor("bob", "remote.test"))
        .await
        .unwrap();
    insert_follow(
        &pool,
        me.id,
        bob.id,
        sakurasato_core::model::FollowState::Accepted,
    )
    .await;

    let raw = issue_token(&pool, "tui").await;
    let state =
        sakurasato_server::state::AppState::from_pool(pool.clone(), make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let body = serde_json::json!({"actor_id": bob.id});
    let resp = app
        .oneshot(
            Request::post("/api/v1/follow")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    assert_eq!(json["already_accepted"], true);
    assert_eq!(json["state"], "accepted");
    assert!(json["delivery_queue_id"].is_null());
    assert!(json["inbox_url"].is_null());

    // delivery_queue 行は 0 (= 既存 accepted のときは再送しない)。
    let count: i64 = sqlx::query_scalar!("SELECT COUNT(*) AS \"c!\" FROM delivery_queue")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

/// 自分自身 (= local actor) を follow しようとすると 409 Conflict。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn follow_rejects_self(pool: PgPool) {
    let me = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);
    let body = serde_json::json!({"actor_id": me.id});
    let resp = app
        .oneshot(
            Request::post("/api/v1/follow")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

/// 入力 body で target を 1 つも指定しない → 400。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn follow_400_when_no_target_specified(pool: PgPool) {
    repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);
    let resp = app
        .oneshot(
            Request::post("/api/v1/follow")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// `actor_id` 経路で存在しない id → 404。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn follow_404_when_actor_id_missing(pool: PgPool) {
    repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);
    let body = serde_json::json!({"actor_id": 99999});
    let resp = app
        .oneshot(
            Request::post("/api/v1/follow")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// `DELETE /api/v1/follow/{id}` で本人 follow → Undo Follow が enqueue され、
/// follow 行は消える。activity.type=Undo, inline Follow object を検証。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn unfollow_enqueues_undo_and_deletes_row(pool: PgPool) {
    let me = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let mut bob = sample_remote_actor("bob", "remote.test");
    bob.shared_inbox_url = Some("https://remote.test/inbox".into());
    let bob = repo::actor::insert(&pool, bob).await.unwrap();
    let follow_id = insert_follow(
        &pool,
        me.id,
        bob.id,
        sakurasato_core::model::FollowState::Accepted,
    )
    .await;

    let raw = issue_token(&pool, "tui").await;
    let state =
        sakurasato_server::state::AppState::from_pool(pool.clone(), make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);
    let resp = app
        .oneshot(
            Request::delete(format!("/api/v1/follow/{follow_id}"))
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    assert_eq!(json["follow_id"], follow_id);
    assert_eq!(json["target_ap_id"], bob.ap_id);
    assert_eq!(json["inbox_url"], "https://remote.test/inbox");
    assert!(json["delivery_queue_id"].is_i64());

    // follow 行は消えた。
    let row = repo::follow::get_by_pair(&pool, me.id, bob.id)
        .await
        .unwrap();
    assert!(row.is_none(), "follow row must be deleted");

    // delivery_queue に Undo Follow が 1 行積まれ、activity.type=Undo。
    let activity: sqlx::types::Json<serde_json::Value> = sqlx::query_scalar!(
        r#"SELECT activity AS "activity: sqlx::types::Json<serde_json::Value>"
           FROM delivery_queue WHERE inbox_url = $1 LIMIT 1"#,
        "https://remote.test/inbox",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let activity = activity.0;
    assert_eq!(activity["type"], "Undo");
    assert_eq!(activity["object"]["type"], "Follow");
    assert_eq!(activity["object"]["actor"], me.ap_id);
    assert_eq!(activity["object"]["object"], bob.ap_id);
}

/// `DELETE /api/v1/follow/{id}` で「他人の follow」を消そうとすると 403。
/// (= 我々が follower でない follow 行をローカル API から触れない)
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn unfollow_403_when_not_owner(pool: PgPool) {
    repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let bob = repo::actor::insert(&pool, sample_remote_actor("bob", "remote.test"))
        .await
        .unwrap();
    let carol = repo::actor::insert(&pool, sample_remote_actor("carol", "other.test"))
        .await
        .unwrap();
    // bob → carol の follow (= 我々の follow ではない)。
    let follow_id = insert_follow(
        &pool,
        bob.id,
        carol.id,
        sakurasato_core::model::FollowState::Accepted,
    )
    .await;

    let raw = issue_token(&pool, "tui").await;
    let state =
        sakurasato_server::state::AppState::from_pool(pool.clone(), make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);
    let resp = app
        .oneshot(
            Request::delete(format!("/api/v1/follow/{follow_id}"))
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // follow 行は残っている (削除されなかったことを確認)。
    let row = repo::follow::get_by_pair(&pool, bob.id, carol.id)
        .await
        .unwrap();
    assert!(row.is_some(), "other-owned follow must not be deleted");
}

/// `DELETE /api/v1/follow/{id}` で存在しない id → 404。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn unfollow_404_when_missing(pool: PgPool) {
    repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);
    let resp = app
        .oneshot(
            Request::delete("/api/v1/follow/99999")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// 認証無しは 401 (= auth middleware が body parse 前に弾く)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn follow_requires_auth(pool: PgPool) {
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);
    let resp = app
        .oneshot(
            Request::post("/api/v1/follow")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"actor_id": 1}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ============================================================
// M13 PR3 (Issue #79) — `GET /api/v1/following` / `/followers` /
// `GET /api/v1/actor/{id}/notes`
// ============================================================

#[allow(clippy::too_many_arguments)] // テストヘルパ; 引数は逐一意味があり束ねづらい。
async fn insert_note_with_visibility(
    pool: &PgPool,
    actor_id: i64,
    host: &str,
    suffix: &str,
    content: &str,
    visibility: sakurasato_core::model::Visibility,
    to_recipients: Vec<String>,
    cc_recipients: Vec<String>,
) -> i64 {
    let ap_id = format!("https://{host}/notes/{suffix}");
    let row = repo::note::insert(
        pool,
        sakurasato_core::repo::note::NewNote {
            ap_id,
            actor_id,
            content: content.into(),
            language: Some("ja".into()),
            in_reply_to_ap_id: None,
            in_reply_to_note_id: None,
            summary: None,
            visibility,
            sensitive: false,
            to_recipients,
            cc_recipients,
            attachments: serde_json::json!([]),
            tags: serde_json::json!([]),
            is_local: false,
            url: None,
            published_at: chrono::Utc::now(),
        },
    )
    .await
    .unwrap();
    row.id
}

/// `GET /api/v1/following` ── accepted のみ返る。pending / rejected は除外。
/// `next_before_id` は `entries` 末尾の `follow.id`。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn following_returns_only_accepted(pool: PgPool) {
    let me = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let bob = repo::actor::insert(&pool, sample_remote_actor("bob", "remote.test"))
        .await
        .unwrap();
    let carol = repo::actor::insert(&pool, sample_remote_actor("carol", "other.test"))
        .await
        .unwrap();
    let dave = repo::actor::insert(&pool, sample_remote_actor("dave", "rej.test"))
        .await
        .unwrap();
    let _f_bob = insert_follow(
        &pool,
        me.id,
        bob.id,
        sakurasato_core::model::FollowState::Accepted,
    )
    .await;
    let _f_carol = insert_follow(
        &pool,
        me.id,
        carol.id,
        sakurasato_core::model::FollowState::Pending,
    )
    .await;
    let _f_dave = insert_follow(
        &pool,
        me.id,
        dave.id,
        sakurasato_core::model::FollowState::Rejected,
    )
    .await;

    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = app
        .oneshot(
            Request::get("/api/v1/following")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    let entries = json["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "only accepted should return: {entries:?}");
    assert_eq!(entries[0]["actor"]["ap_id"], bob.ap_id);
    assert_eq!(entries[0]["follow_state"], "accepted");
    assert!(entries[0]["follow_id"].is_i64());
    // 秘密鍵は出ない (二重防御の確認)。
    let body = serde_json::to_string(&json).unwrap();
    assert!(!body.contains("private_key"), "private_key leaked: {body}");
    // next_before_id = 末尾 (= 唯一) の follow_id と一致。
    assert_eq!(json["next_before_id"], entries[0]["follow_id"]);
}

/// `GET /api/v1/followers` も対称。bob → alice (accepted) のみ返る。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn followers_returns_only_accepted(pool: PgPool) {
    let me = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let bob = repo::actor::insert(&pool, sample_remote_actor("bob", "remote.test"))
        .await
        .unwrap();
    let carol = repo::actor::insert(&pool, sample_remote_actor("carol", "other.test"))
        .await
        .unwrap();
    // bob → alice accepted、carol → alice pending。
    let _ = insert_follow(
        &pool,
        bob.id,
        me.id,
        sakurasato_core::model::FollowState::Accepted,
    )
    .await;
    let _ = insert_follow(
        &pool,
        carol.id,
        me.id,
        sakurasato_core::model::FollowState::Pending,
    )
    .await;

    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = app
        .oneshot(
            Request::get("/api/v1/followers")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    let entries = json["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "only accepted should return: {entries:?}");
    assert_eq!(entries[0]["actor"]["ap_id"], bob.ap_id);
}

/// `GET /api/v1/following?limit=&before_id=` のページネーション。
/// 3 件登録 → limit=2 で 2 件 → `next_before_id` で次ページに 1 件残る。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn following_pagination_uses_follow_id_cursor(pool: PgPool) {
    let me = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let bob = repo::actor::insert(&pool, sample_remote_actor("bob", "b.test"))
        .await
        .unwrap();
    let carol = repo::actor::insert(&pool, sample_remote_actor("carol", "c.test"))
        .await
        .unwrap();
    let dave = repo::actor::insert(&pool, sample_remote_actor("dave", "d.test"))
        .await
        .unwrap();
    // 順序: bob → carol → dave (= follow.id 昇順)。
    let _ = insert_follow(
        &pool,
        me.id,
        bob.id,
        sakurasato_core::model::FollowState::Accepted,
    )
    .await;
    let _ = insert_follow(
        &pool,
        me.id,
        carol.id,
        sakurasato_core::model::FollowState::Accepted,
    )
    .await;
    let _ = insert_follow(
        &pool,
        me.id,
        dave.id,
        sakurasato_core::model::FollowState::Accepted,
    )
    .await;

    let raw = issue_token(&pool, "tui").await;
    let state =
        sakurasato_server::state::AppState::from_pool(pool.clone(), make_config("example.test"));
    let app = sakurasato_server::local_api::router(state.clone());

    let resp = app
        .oneshot(
            Request::get("/api/v1/following?limit=2")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    let entries = json["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    // 並び順は follow.id DESC ── 最後に follow した dave が先頭、その次が carol。
    assert_eq!(entries[0]["actor"]["ap_id"], dave.ap_id);
    assert_eq!(entries[1]["actor"]["ap_id"], carol.ap_id);
    let next = json["next_before_id"].as_i64().unwrap();

    // 次ページ: before_id=next で残り 1 件 (bob)。
    let app = sakurasato_server::local_api::router(state);
    let resp = app
        .oneshot(
            Request::get(format!("/api/v1/following?limit=2&before_id={next}"))
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    let entries = json["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["actor"]["ap_id"], bob.ap_id);
}

/// `GET /api/v1/following` ローカル actor 未 init → 503。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn following_503_when_local_actor_missing(pool: PgPool) {
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = app
        .oneshot(
            Request::get("/api/v1/following")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}

/// `GET /api/v1/following` 認証無しは 401。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn following_requires_auth(pool: PgPool) {
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);
    let resp = app
        .oneshot(
            Request::get("/api/v1/following")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// `GET /api/v1/actor/{id}/notes` ── public / unlisted は誰でも (= 自分も)
/// 見える、followers は accepted フォロワーだけ、direct は宛先のみ。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn actor_notes_applies_visibility_filter(pool: PgPool) {
    let me = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let bob = repo::actor::insert(&pool, sample_remote_actor("bob", "remote.test"))
        .await
        .unwrap();

    // alice が bob を未フォロー (= follow 行なし) の状態でテスト開始。
    // bob の投稿:
    //   - public  → 見える
    //   - unlisted → 見える
    //   - followers → 見えない (フォロー未確立)
    //   - direct (to me)  → 見える
    //   - direct (to 別人) → 見えない
    let public_id = insert_note_with_visibility(
        &pool,
        bob.id,
        "remote.test",
        "p",
        "public note",
        sakurasato_core::model::Visibility::Public,
        vec!["https://www.w3.org/ns/activitystreams#Public".into()],
        vec![],
    )
    .await;
    let unlisted_id = insert_note_with_visibility(
        &pool,
        bob.id,
        "remote.test",
        "u",
        "unlisted note",
        sakurasato_core::model::Visibility::Unlisted,
        vec![],
        vec!["https://www.w3.org/ns/activitystreams#Public".into()],
    )
    .await;
    let followers_id = insert_note_with_visibility(
        &pool,
        bob.id,
        "remote.test",
        "f",
        "followers note",
        sakurasato_core::model::Visibility::Followers,
        vec![format!("{}/followers", bob.ap_id)],
        vec![],
    )
    .await;
    let direct_to_me_id = insert_note_with_visibility(
        &pool,
        bob.id,
        "remote.test",
        "d-me",
        "direct to alice",
        sakurasato_core::model::Visibility::Direct,
        vec![me.ap_id.clone()],
        vec![],
    )
    .await;
    let direct_other_id = insert_note_with_visibility(
        &pool,
        bob.id,
        "remote.test",
        "d-other",
        "direct to someone else",
        sakurasato_core::model::Visibility::Direct,
        vec!["https://other.test/users/eve".into()],
        vec![],
    )
    .await;

    let raw = issue_token(&pool, "tui").await;
    let state =
        sakurasato_server::state::AppState::from_pool(pool.clone(), make_config("example.test"));
    let app = sakurasato_server::local_api::router(state.clone());

    let resp = app
        .oneshot(
            Request::get(format!("/api/v1/actor/{}/notes", bob.id))
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    let ids: Vec<i64> = json["notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_i64().unwrap())
        .collect();
    assert!(
        ids.contains(&public_id),
        "public should be visible: {ids:?}"
    );
    assert!(
        ids.contains(&unlisted_id),
        "unlisted should be visible: {ids:?}"
    );
    assert!(
        !ids.contains(&followers_id),
        "followers should be hidden (no follow): {ids:?}",
    );
    assert!(
        ids.contains(&direct_to_me_id),
        "direct to me should be visible: {ids:?}",
    );
    assert!(
        !ids.contains(&direct_other_id),
        "direct to someone else must be hidden: {ids:?}",
    );

    // 次に alice が bob を accepted で follow する → followers が見えるようになる。
    let _ = insert_follow(
        &pool,
        me.id,
        bob.id,
        sakurasato_core::model::FollowState::Accepted,
    )
    .await;
    let app = sakurasato_server::local_api::router(state);
    let resp = app
        .oneshot(
            Request::get(format!("/api/v1/actor/{}/notes", bob.id))
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    let ids: Vec<i64> = json["notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_i64().unwrap())
        .collect();
    assert!(
        ids.contains(&followers_id),
        "followers should be visible after follow: {ids:?}",
    );
}

/// `GET /api/v1/actor/{id}/notes` author == viewer (= 自分自身) のとき、
/// direct も含めた全件返る。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn actor_notes_author_sees_all_own(pool: PgPool) {
    let me = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let direct_id = insert_note_with_visibility(
        &pool,
        me.id,
        "example.test",
        "self-direct",
        "secret",
        sakurasato_core::model::Visibility::Direct,
        vec!["https://other.test/users/eve".into()], // 自分宛ではないが author なので見える
        vec![],
    )
    .await;
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = app
        .oneshot(
            Request::get(format!("/api/v1/actor/{}/notes", me.id))
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    let ids: Vec<i64> = json["notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_i64().unwrap())
        .collect();
    assert!(
        ids.contains(&direct_id),
        "author should see all own notes incl. direct: {ids:?}",
    );
}

/// `GET /api/v1/actor/{id}/notes` 存在しない actor → 404。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn actor_notes_404_when_target_missing(pool: PgPool) {
    repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = app
        .oneshot(
            Request::get("/api/v1/actor/99999/notes")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// `GET /api/v1/actor/{id}/notes` ローカル actor 未 init → 503。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn actor_notes_503_when_local_actor_missing(pool: PgPool) {
    let bob = repo::actor::insert(&pool, sample_remote_actor("bob", "remote.test"))
        .await
        .unwrap();
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = app
        .oneshot(
            Request::get(format!("/api/v1/actor/{}/notes", bob.id))
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}

/// `GET /api/v1/actor/{id}/notes?limit=&before_id=` のページネーション。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn actor_notes_pagination(pool: PgPool) {
    let me = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let bob = repo::actor::insert(&pool, sample_remote_actor("bob", "remote.test"))
        .await
        .unwrap();
    let mut ids = Vec::new();
    for i in 0..3 {
        let id = insert_note_with_visibility(
            &pool,
            bob.id,
            "remote.test",
            &format!("p{i}"),
            "x",
            sakurasato_core::model::Visibility::Public,
            vec!["https://www.w3.org/ns/activitystreams#Public".into()],
            vec![],
        )
        .await;
        ids.push(id);
    }

    let raw = issue_token(&pool, "tui").await;
    let state =
        sakurasato_server::state::AppState::from_pool(pool.clone(), make_config("example.test"));
    let _ = me;
    let app = sakurasato_server::local_api::router(state.clone());

    let resp = app
        .oneshot(
            Request::get(format!("/api/v1/actor/{}/notes?limit=2", bob.id))
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    let notes = json["notes"].as_array().unwrap();
    assert_eq!(notes.len(), 2);
    // note.id DESC 並び (= 最新が先頭)。
    assert_eq!(notes[0]["id"].as_i64().unwrap(), ids[2]);
    assert_eq!(notes[1]["id"].as_i64().unwrap(), ids[1]);
    let next = json["next_before_id"].as_i64().unwrap();
    assert_eq!(next, ids[1]);

    let app = sakurasato_server::local_api::router(state);
    let resp = app
        .oneshot(
            Request::get(format!(
                "/api/v1/actor/{}/notes?limit=2&before_id={next}",
                bob.id,
            ))
            .header(header::AUTHORIZATION, format!("Bearer {raw}"))
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    let notes = json["notes"].as_array().unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0]["id"].as_i64().unwrap(), ids[0]);
}

/// `GET /api/v1/actor/{id}/notes` 認証無しは 401。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn actor_notes_requires_auth(pool: PgPool) {
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);
    let resp = app
        .oneshot(
            Request::get("/api/v1/actor/1/notes")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ─── #206 PR3: in-app 通知の local API 一覧 + 一括既読 ──────────────────────

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notifications_list_and_mark_all_read(pool: PgPool) {
    use sakurasato_core::repo::notification::{self, NewNotification};

    let alice = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let bob = repo::actor::insert(&pool, common::sample_local_actor("bob", "remote.test"))
        .await
        .unwrap();
    notification::insert(
        &pool,
        NewNotification {
            recipient_actor_id: alice.id,
            event_type: "follow".into(),
            notifier_actor_id: Some(bob.id),
            note_id: None,
            reaction: None,
            created_at: chrono::Utc::now(),
        },
    )
    .await
    .unwrap();

    let raw = issue_token(&pool, "tui").await;
    let state =
        sakurasato_server::state::AppState::from_pool(pool.clone(), make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    // list → unread 1、shape 確認。
    let resp = app
        .clone()
        .oneshot(
            Request::get("/api/v1/notifications")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    assert_eq!(json["unread_count"], 1);
    assert_eq!(json["items"][0]["event_type"], "follow");
    assert_eq!(json["items"][0]["is_read"], false);
    assert!(json["items"][0]["notifier_acct"].is_string());

    // mark-all-read → 204。
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/v1/notifications/mark-all-read")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // list again → unread 0、既読化。
    let resp = app
        .oneshot(
            Request::get("/api/v1/notifications")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let json = read_json(resp).await;
    assert_eq!(json["unread_count"], 0);
    assert_eq!(json["items"][0]["is_read"], true);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notifications_requires_token(pool: PgPool) {
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);
    let resp = app
        .oneshot(
            Request::get("/api/v1/notifications")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
