//! M4 PR1 統合テスト: `GET /media/{*key}` パストラバーサル防御。
//!
//! S3 が実機に居ないテスト環境では正常系 (versitygw に PUT して GET で取る)
//! は走らせられないので、本ファイルでは **handler 自身による key 検証** が
//! S3 client 呼び出し **前** に効くことだけ確認する。
//! - `/media/../secret` → 400 (S3 へ到達する前に弾く)
//! - `/media/a%2F..%2Fb` → axum が `a/../b` にデコードし 400
//! - `/media/` → そもそも `{*key}` がマッチしないので 404
//!
//! 正常系 (200 + body) は compose 統合テスト (今後の milestone) で網羅する。

#![forbid(unsafe_code)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use sqlx::PgPool;
use tower::ServiceExt;

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

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn media_rejects_dotdot_traversal(pool: PgPool) {
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::routes::router(state);

    let resp = app
        .oneshot(
            Request::get("/media/a/../../secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // is_safe_key が S3 呼び出しの前に弾くので 400。S3 まで届いていれば
    // 接続失敗で 500 になるはずなので、400 が返ることで「key 検証が手前で
    // 動いている」ことが確認できる。
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn media_rejects_url_encoded_traversal(pool: PgPool) {
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::routes::router(state);

    // `%2F` は `/`、`%2E%2E` は `..`。axum が percent-decode してから path
    // 抽出するため、is_safe_key の `..` セグメント検出にかかる。
    let resp = app
        .oneshot(
            Request::get("/media/a%2F%2E%2E%2Fb")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn media_rejects_double_slash(pool: PgPool) {
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::routes::router(state);

    let resp = app
        .oneshot(Request::get("/media/a//b.png").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
