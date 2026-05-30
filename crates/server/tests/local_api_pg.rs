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
