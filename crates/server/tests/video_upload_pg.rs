//! 動画アップロード統合テスト: `POST /api/v1/media` の `video/*` 分岐経路。
//!
//! 実 media-proxy には接続しない (= `media_upload_pg.rs` と同方針)。
//! `Content-Type: video/*` ヒットによる分岐、`kind` ガード、
//! `media_proxy.video.max_bytes` に基づく上限判定など、本体側のロジックを
//! 覆う。実際の `media-proxy /v1/video/sanitize` 経路は手動 E2E で確認する。

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
            socket: "/tmp/dead-media-proxy.sock".into(),
            max_bytes: 4 * 1024 * 1024,
            max_pixels: 16_000_000,
            video: sakurasato_core::config::VideoConfig {
                max_bytes: 1_000,
                max_duration_secs: 300,
            },
            emoji_import: sakurasato_core::config::EmojiImportConfig::default(),
        },
        miauth: None,
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

/// `kind=avatar` + `Content-Type: video/mp4` は avatar/header が画像専用の
/// ため 400 で弾かれる (`upload_video_core` の `kind != attachment` ガード)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn video_rejected_for_avatar_kind(pool: PgPool) {
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
                .header(header::CONTENT_TYPE, "video/mp4")
                .body(Body::from(vec![0u8; 16]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = read_json(resp).await;
    assert!(body["error"].as_str().unwrap().contains("attachment"));
}

/// `kind=header` も同様に動画を拒否する。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn video_rejected_for_header_kind(pool: PgPool) {
    repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);
    let resp = app
        .oneshot(
            Request::post("/api/v1/media?kind=header")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "video/webm")
                .body(Body::from(vec![0u8; 16]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// `media_proxy.video.max_bytes` (このテストでは 1000 バイト) 超過は
/// `upload_video_core` 側の明示チェックで 413 になる。router 層の
/// `DefaultBodyLimit` は画像/動画上限の大きい方に揃えてあるため、ここでは
/// ハンドラ内バリデーションの経路を通す。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn video_payload_exceeds_video_max_bytes(pool: PgPool) {
    repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);
    let resp = app
        .oneshot(
            Request::post("/api/v1/media?kind=attachment")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "video/mp4")
                .body(Body::from(vec![0u8; 2000]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

/// `kind=attachment` + 上限以内の動画は media-proxy に到達し、そこで
/// socket 未接続のため 502 になる (= 早期 validation を通過後の経路)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn video_reaches_media_proxy_and_fails_with_bad_gateway(pool: PgPool) {
    repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);
    let resp = app
        .oneshot(
            Request::post("/api/v1/media?kind=attachment")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "video/webm")
                .body(Body::from(vec![0u8; 16]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
}

/// `Content-Type` が `video/*` で始まらなければ画像経路に入る。
/// 動画バイト列を image variant として media-proxy に投げようとするが、
/// 同じく socket 未接続で 502 になる (= 分岐そのものが正しく効いている
/// ことの回帰テスト。もし分岐が壊れて `kind=attachment` の動画が誤って
/// 画像経路に入っても、この経路とステータスコードだけでは区別できない
/// ため、上の `video_reaches_media_proxy_and_fails_with_bad_gateway` と
/// 対にして `kind=avatar` ガード [`video_rejected_for_avatar_kind`] で
/// 分岐の実在を担保する)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn non_video_content_type_uses_image_path(pool: PgPool) {
    repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);
    let resp = app
        .oneshot(
            Request::post("/api/v1/media?kind=attachment")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/octet-stream")
                .body(Body::from(vec![0u8; 16]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
}
