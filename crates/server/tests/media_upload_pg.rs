//! M7 統合テスト: `POST /api/v1/media` (画像アップロード)、`PATCH /api/v1/actor/profile`、
//! および `POST /api/v1/notes` の `attachment_ids` 経路。
//!
//! 実 media-proxy には接続しない。validation / 認証 / DB 整合 (dedup,
//! ownership) など本体側のロジックを覆う。実際の `media-proxy → versitygw`
//! 経路は手動 E2E (`docker compose up` + curl) で確認する。

#![forbid(unsafe_code)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use sakurasato_core::repo;
use serde_json::json;
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
            socket: "/tmp/dead-media-proxy.sock".into(),
            max_bytes: 4 * 1024 * 1024,
            max_pixels: 16_000_000,
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

async fn read_json(resp: axum::response::Response) -> serde_json::Value {
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null)
}

// ────────── POST /api/v1/media ──────────

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn upload_requires_auth(pool: PgPool) {
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);
    let resp = app
        .oneshot(
            Request::post("/api/v1/media?kind=avatar")
                .body(Body::from(vec![0u8; 8]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn upload_rejects_unknown_kind(pool: PgPool) {
    repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);
    let resp = app
        .oneshot(
            Request::post("/api/v1/media?kind=banner")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::from(vec![0u8; 8]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = read_json(resp).await;
    assert!(body["error"].as_str().unwrap().contains("kind"));
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn upload_rejects_empty_body(pool: PgPool) {
    repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);
    let resp = app
        .oneshot(
            Request::post("/api/v1/media?kind=avatar")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn upload_returns_503_without_local_actor(pool: PgPool) {
    // local actor が居ない状態: kind が valid でも 503 で落ちる経路は
    // 「resolve_local_actor 前に kind 検査が走る」順序でテストするため、
    // body は非空に、kind=avatar、actor は insert しない。
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);
    let resp = app
        .oneshot(
            Request::post("/api/v1/media?kind=avatar")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::from(vec![0u8; 8]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn upload_502_when_media_proxy_unreachable(pool: PgPool) {
    // media-proxy socket は存在しないパスを指す (= make_config の既定)。
    // 接続失敗で 502 になることを確認 (= 早期 validation を通過後に
    // media-proxy へ実際に投げて失敗するパスの検証)。
    repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);
    let resp = app
        .oneshot(
            Request::post("/api/v1/media?kind=avatar")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::from(vec![0u8; 8]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn upload_payload_too_large(pool: PgPool) {
    repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let raw = issue_token(&pool, "tui").await;
    let mut config = make_config("example.test");
    config.media_proxy.max_bytes = 100;
    let state = sakurasato_server::state::AppState::from_pool(pool, config);
    let app = sakurasato_server::local_api::router(state);
    // 100 バイト超 = 上限超え。router 層の DefaultBodyLimit (= max_bytes と
    // 同期) で先に 413 になる経路。
    let resp = app
        .oneshot(
            Request::post("/api/v1/media?kind=avatar")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::from(vec![0u8; 200]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

// ────────── PATCH /api/v1/actor/profile ──────────

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn profile_requires_auth(pool: PgPool) {
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);
    let resp = app
        .oneshot(
            Request::patch("/api/v1/actor/profile")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}".to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn profile_updates_display_name(pool: PgPool) {
    repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let raw = issue_token(&pool, "tui").await;
    let state =
        sakurasato_server::state::AppState::from_pool(pool.clone(), make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);
    let body = json!({ "display_name": "ありす" }).to_string();
    let resp = app
        .oneshot(
            Request::patch("/api/v1/actor/profile")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_json(resp).await;
    assert_eq!(body["display_name"].as_str(), Some("ありす"));
    assert_eq!(body["queued_deliveries"].as_u64(), Some(0));

    // 永続化されているか DB 側でも確認する。
    let row = repo::actor::get_by_ap_id(&pool, "https://example.test/users/alice")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.display_name.as_deref(), Some("ありす"));
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn profile_rejects_overlapping_clear_and_value(pool: PgPool) {
    repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);
    let body = json!({ "display_name": "x", "clear_display_name": true }).to_string();
    let resp = app
        .oneshot(
            Request::patch("/api/v1/actor/profile")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn profile_rejects_unknown_media_id(pool: PgPool) {
    repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);
    let body = json!({ "icon_media_id": 9999 }).to_string();
    let resp = app
        .oneshot(
            Request::patch("/api/v1/actor/profile")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn profile_attaches_avatar_from_media_id(pool: PgPool) {
    let actor = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let media = repo::media::insert(
        &pool,
        repo::media::NewMedia {
            storage_key: "abc.webp".into(),
            media_type: "image/webp".into(),
            width: 128,
            height: 128,
            byte_size: 1024,
            kind: "avatar".into(),
            alt_text: None,
            owner_actor_id: actor.id,
        },
    )
    .await
    .unwrap();
    let raw = issue_token(&pool, "tui").await;
    let state =
        sakurasato_server::state::AppState::from_pool(pool.clone(), make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);
    let body = json!({ "icon_media_id": media.id }).to_string();
    let resp = app
        .oneshot(
            Request::patch("/api/v1/actor/profile")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_json(resp).await;
    assert_eq!(
        body["icon_url"].as_str(),
        Some("https://example.test/media/abc.webp")
    );
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn profile_rejects_wrong_kind_for_icon(pool: PgPool) {
    let actor = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    // kind=attachment は icon 用ではないので 400 を返す。
    let media = repo::media::insert(
        &pool,
        repo::media::NewMedia {
            storage_key: "abc.webp".into(),
            media_type: "image/webp".into(),
            width: 128,
            height: 128,
            byte_size: 1024,
            kind: "attachment".into(),
            alt_text: None,
            owner_actor_id: actor.id,
        },
    )
    .await
    .unwrap();
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);
    let body = json!({ "icon_media_id": media.id }).to_string();
    let resp = app
        .oneshot(
            Request::patch("/api/v1/actor/profile")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ────────── POST /api/v1/notes attachment_ids ──────────

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_attach_owned_media(pool: PgPool) {
    let actor = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let media = repo::media::insert(
        &pool,
        repo::media::NewMedia {
            storage_key: "xyz.webp".into(),
            media_type: "image/webp".into(),
            width: 800,
            height: 600,
            byte_size: 4096,
            kind: "attachment".into(),
            alt_text: Some("a cat".into()),
            owner_actor_id: actor.id,
        },
    )
    .await
    .unwrap();
    let raw = issue_token(&pool, "tui").await;
    let state =
        sakurasato_server::state::AppState::from_pool(pool.clone(), make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);
    let req_body = json!({
        "content": "hi",
        "attachment_ids": [media.id],
    })
    .to_string();
    let resp = app
        .oneshot(
            Request::post("/api/v1/notes")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(req_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = read_json(resp).await;
    let note_id = body["id"].as_i64().unwrap();
    // media 行が紐付いた状態
    let row = repo::media::get_by_id(&pool, media.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.note_id, Some(note_id));
    // note.attachments JSONB に Document が並ぶ
    let note = repo::note::get_by_id(&pool, note_id)
        .await
        .unwrap()
        .unwrap();
    let attachments = note.attachments.0.as_array().unwrap();
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0]["type"], "Document");
    assert_eq!(
        attachments[0]["url"].as_str(),
        Some("https://example.test/media/xyz.webp")
    );
    assert_eq!(attachments[0]["name"].as_str(), Some("a cat"));
    assert_eq!(attachments[0]["width"].as_i64(), Some(800));
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_reject_double_attach(pool: PgPool) {
    let actor = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let media = repo::media::insert(
        &pool,
        repo::media::NewMedia {
            storage_key: "xyz.webp".into(),
            media_type: "image/webp".into(),
            width: 800,
            height: 600,
            byte_size: 4096,
            kind: "attachment".into(),
            alt_text: None,
            owner_actor_id: actor.id,
        },
    )
    .await
    .unwrap();
    let raw = issue_token(&pool, "tui").await;
    let state =
        sakurasato_server::state::AppState::from_pool(pool.clone(), make_config("example.test"));
    let app = sakurasato_server::local_api::router(state.clone());
    let req_body = json!({
        "content": "first",
        "attachment_ids": [media.id],
    })
    .to_string();
    let resp = app
        .oneshot(
            Request::post("/api/v1/notes")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(req_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // 二度目: 既に紐付いた media を使うと 400
    let app2 = sakurasato_server::local_api::router(state);
    let req_body2 = json!({
        "content": "second",
        "attachment_ids": [media.id],
    })
    .to_string();
    let resp2 = app2
        .oneshot(
            Request::post("/api/v1/notes")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(req_body2))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp2.status(), StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_reject_too_many_attachments(pool: PgPool) {
    let actor = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let mut ids = Vec::new();
    for i in 0..5 {
        let m = repo::media::insert(
            &pool,
            repo::media::NewMedia {
                storage_key: format!("attach-{i}.webp"),
                media_type: "image/webp".into(),
                width: 100,
                height: 100,
                byte_size: 1,
                kind: "attachment".into(),
                alt_text: None,
                owner_actor_id: actor.id,
            },
        )
        .await
        .unwrap();
        ids.push(m.id);
    }
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);
    let req_body = json!({
        "content": "x",
        "attachment_ids": ids,
    })
    .to_string();
    let resp = app
        .oneshot(
            Request::post("/api/v1/notes")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(req_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_reject_foreign_media(pool: PgPool) {
    let local = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    // 別 actor を作って、その所有 media を attach しようとする経路。
    let remote = sakurasato_core::repo::actor::NewActor {
        ap_id: "https://other.test/users/bob".into(),
        preferred_username: "bob".into(),
        host: "other.test".into(),
        display_name: None,
        summary: None,
        icon_url: None,
        image_url: None,
        inbox_url: "https://other.test/users/bob/inbox".into(),
        shared_inbox_url: None,
        outbox_url: None,
        followers_url: None,
        following_url: None,
        public_key_id: "https://other.test/users/bob#main-key".into(),
        public_key_pem: "MOCK".into(),
        private_key_pem: None,
        ed25519_public_key_id: None,
        ed25519_public_key_pem: None,
        ed25519_private_key_pem: None,
        also_known_as: vec![],
        moved_to_ap_id: None,
        is_local: false,
        actor_type: "Person".into(),
    };
    let remote_row = repo::actor::insert(&pool, remote).await.unwrap();
    let foreign_media = repo::media::insert(
        &pool,
        repo::media::NewMedia {
            storage_key: "evil.webp".into(),
            media_type: "image/webp".into(),
            width: 100,
            height: 100,
            byte_size: 1,
            kind: "attachment".into(),
            alt_text: None,
            owner_actor_id: remote_row.id,
        },
    )
    .await
    .unwrap();
    let _ = local; // local actor は会話には使うがここでは config 解決で参照される
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);
    let req_body = json!({
        "content": "stolen",
        "attachment_ids": [foreign_media.id],
    })
    .to_string();
    let resp = app
        .oneshot(
            Request::post("/api/v1/notes")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(req_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}
