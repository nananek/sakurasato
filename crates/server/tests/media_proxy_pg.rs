//! M6 統合テスト: `GET /api/v1/media/proxy?url=&variant=`。
//!
//! 認証 + 早期検証 (URL parse / SSRF / variant) の経路は本テストで覆う。
//! 実 socket への接続失敗 (= media-proxy が起動していない) → 502 系も
//! `from_pool` を使って実際に試す。実 media-proxy を立てる E2E は
//! `crates/media-proxy/tests/` 側で別途確認する。

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

fn make_config(host: &str, media_socket: &str) -> sakurasato_core::Config {
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
            socket: media_socket.into(),
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

fn url_encode(s: &str) -> String {
    // テスト用最小実装。`?` `&` `=` `:` `/` を encode する。
    // 厳密には form_urlencoded だが、テスト URL は ASCII 限定なので簡易で十分。
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// テスト用に「絶対存在しない socket パス」を作る。tempfile を使うと
/// `Drop` でファイルが消えるが、本テストでは「**接続が失敗する** こと」自体を
/// 確かめたいので、消えても OK / 存在しなければなお OK。
fn dead_socket_path() -> String {
    format!(
        "/tmp/sakurasato-media-proxy-test-{}.sock",
        std::process::id()
    )
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn proxy_requires_auth(pool: PgPool) {
    let state = sakurasato_server::state::AppState::from_pool(
        pool,
        make_config("example.test", "/tmp/x.sock"),
    );
    let app = sakurasato_server::local_api::router(state);

    let resp = app
        .oneshot(
            Request::get("/api/v1/media/proxy?url=https%3A%2F%2Fexample.com%2Fa.png")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn proxy_rejects_invalid_url_early(pool: PgPool) {
    repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(
        pool,
        make_config("example.test", "/tmp/x.sock"),
    );
    let app = sakurasato_server::local_api::router(state);

    let path = format!("/api/v1/media/proxy?url={}", url_encode("not a url"));
    let resp = app
        .oneshot(
            Request::get(&path)
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = read_json(resp).await;
    assert!(json["error"].as_str().unwrap().contains("invalid url"));
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn proxy_rejects_non_http_scheme(pool: PgPool) {
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(
        pool,
        make_config("example.test", "/tmp/x.sock"),
    );
    let app = sakurasato_server::local_api::router(state);

    let path = format!(
        "/api/v1/media/proxy?url={}",
        url_encode("file:///etc/passwd")
    );
    let resp = app
        .oneshot(
            Request::get(&path)
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn proxy_blocks_ssrf_targets_server_side(pool: PgPool) {
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(
        pool,
        make_config("example.test", "/tmp/x.sock"),
    );
    let app = sakurasato_server::local_api::router(state);

    for url in [
        "http://127.0.0.1/x",
        "http://10.0.0.1/x",
        "http://169.254.169.254/latest/meta-data/",
        "http://localhost/x",
        "http://postgres.local/x",
    ] {
        let path = format!("/api/v1/media/proxy?url={}", url_encode(url));
        let resp = app
            .clone()
            .oneshot(
                Request::get(&path)
                    .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{url}");
    }
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn proxy_rejects_invalid_variant(pool: PgPool) {
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(
        pool,
        make_config("example.test", "/tmp/x.sock"),
    );
    let app = sakurasato_server::local_api::router(state);

    let path = format!(
        "/api/v1/media/proxy?url={}&variant=banner",
        url_encode("https://example.com/a.png")
    );
    let resp = app
        .oneshot(
            Request::get(&path)
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = read_json(resp).await;
    assert!(json["error"].as_str().unwrap().contains("variant"));
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn proxy_returns_bad_gateway_when_socket_missing(pool: PgPool) {
    // 実 media-proxy が居ない経路 (起動忘れ / clean restart 中) は 502 にする。
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(
        pool,
        make_config("example.test", &dead_socket_path()),
    );
    let app = sakurasato_server::local_api::router(state);

    let path = format!(
        "/api/v1/media/proxy?url={}",
        url_encode("https://example.com/a.png")
    );
    let resp = app
        .oneshot(
            Request::get(&path)
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // 接続エラーは Transport → BAD_GATEWAY にマップされる。
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
}
