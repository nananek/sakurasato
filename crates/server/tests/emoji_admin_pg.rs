//! TUI 絵文字管理画面向けローカル API の統合テスト (Issue #328 系)。
//!
//! - `POST /api/v1/emojis/import` ── zip 全体の形式検証 (壊れた zip / 空 body)。
//!   実 media-proxy が居ないテスト環境では正常系の `imported > 0` は検証
//!   できない (`import_archive` 内の `sanitize_image` が Unix socket 接続
//!   失敗で `Err` になり、`summary.failed` にカウントされる) ── これは
//!   `media_pg.rs` が正常系 GET を実機無しでは検証していないのと同じ制約。
//!   代わりに「有効な zip でもエンドポイントとしては 200 + failed カウント
//!   で返る」ことまでを検証する。
//! - `GET /api/v1/emojis/remote?q=&limit=` ── DB キャッシュ済みリモート絵文字
//!   の検索。fetch 失敗キャッシュ (`image_key IS NULL`) を除外すること。
//! - `POST /api/v1/emojis/local/from-remote` ── エラーパス (404/400/409)。
//!   正常系の S3 GET/PUT は実 versitygw が無いテスト環境では検証できない
//!   (`media_pg.rs` と同じ制約)。

#![forbid(unsafe_code)]

use std::io::{Cursor, Write};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use sakurasato_core::repo;
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
            // 存在しないソケット。テストは media-proxy 接続失敗経路をあえて使う。
            socket: "/tmp/sakurasato-emoji-admin-test-nonexistent.sock".into(),
            max_bytes: 1024,
            max_pixels: 1024,
            video: sakurasato_core::config::VideoConfig::default(),
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
    serde_json::from_slice(&body).unwrap()
}

async fn insert_local_emoji(pool: &PgPool, shortcode: &str) -> i64 {
    let row = repo::emoji::upsert_local(
        pool,
        repo::emoji::NewLocalEmoji {
            shortcode: shortcode.into(),
            category: None,
            aliases: vec![],
            image_key: format!("emoji/local/{shortcode}.webp"),
            media_type: "image/webp".into(),
            license: None,
            is_sensitive: false,
        },
    )
    .await
    .unwrap();
    row.id
}

/// `image_key = None` は fetch 失敗キャッシュ (= コピー元になれない) を再現する。
async fn insert_remote_emoji(
    pool: &PgPool,
    shortcode: &str,
    host: &str,
    image_key: Option<&str>,
) -> i64 {
    let row = repo::emoji::upsert_remote(
        pool,
        repo::emoji::NewRemoteEmoji {
            shortcode: shortcode.into(),
            ap_id: format!("https://{host}/emojis/{shortcode}"),
            host: host.into(),
            image_key: image_key.map(String::from),
            media_type: "image/webp".into(),
            last_failed_at: if image_key.is_none() {
                Some(chrono::Utc::now())
            } else {
                None
            },
        },
    )
    .await
    .unwrap();
    row.id
}

/// `emoji_import.rs` のテストヘルパと同型: メモリ上に最小 Misskey zip を組み立てる。
fn build_zip(meta_json: &str, extras: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buf = Cursor::new(Vec::<u8>::new());
    {
        let mut zw = zip::ZipWriter::new(&mut buf);
        let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zw.start_file("meta.json", opts).unwrap();
        zw.write_all(meta_json.as_bytes()).unwrap();
        for (name, bytes) in extras {
            zw.start_file(*name, opts).unwrap();
            zw.write_all(bytes).unwrap();
        }
        zw.finish().unwrap();
    }
    buf.into_inner()
}

async fn post_import(app: axum::Router, body: Vec<u8>, token: &str) -> axum::response::Response {
    let req = Request::post("/api/v1/emojis/import")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from(body))
        .unwrap();
    app.oneshot(req).await.unwrap()
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn import_rejects_empty_body(pool: PgPool) {
    let token = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = post_import(app, Vec::new(), &token).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn import_rejects_invalid_zip(pool: PgPool) {
    let token = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = post_import(app, b"not a zip file at all".to_vec(), &token).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn import_valid_zip_without_media_proxy_counts_as_failed(pool: PgPool) {
    // media-proxy が居ない (= socket 接続失敗) ため sanitize は必ず失敗する。
    // それでもエンドポイント自体は 200 を返し、失敗件数を summary で表現する
    // ことを確認する (= CLI と違い 400 にはしない、local_api::emoji_admin::import 参照)。
    let raw = r#"{
        "metaVersion": 2,
        "emojis": [
            {"downloaded": true, "fileName": "blob.png",
             "emoji": {"name": "blob_party", "aliases": []}}
        ]
    }"#;
    let zip_bytes = build_zip(raw, &[("blob.png", b"fake-png-bytes")]);
    let token = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = post_import(app, zip_bytes, &token).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    assert_eq!(json["imported"], 0);
    assert_eq!(json["failed"], 1);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn search_remote_excludes_uncached_and_local(pool: PgPool) {
    let cached = insert_remote_emoji(
        &pool,
        "blobcat",
        "misskey.example",
        Some("emoji/remote/misskey.example/blobcat.webp"),
    )
    .await;
    insert_remote_emoji(&pool, "uncached", "misskey.example", None).await;
    insert_local_emoji(&pool, "blobcat").await;
    let token = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = app
        .oneshot(
            Request::get("/api/v1/emojis/remote?limit=20")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    let items = json["items"].as_array().expect("items array");
    assert_eq!(
        items.len(),
        1,
        "only the cached remote emoji; got {items:?}"
    );
    assert_eq!(items[0]["id"], cached);
    assert_eq!(items[0]["host"], "misskey.example");
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn search_remote_query_matches_shortcode_and_host(pool: PgPool) {
    insert_remote_emoji(
        &pool,
        "blobcat",
        "misskey.example",
        Some("emoji/remote/misskey.example/blobcat.webp"),
    )
    .await;
    insert_remote_emoji(
        &pool,
        "partying",
        "other.example",
        Some("emoji/remote/other.example/partying.webp"),
    )
    .await;
    let token = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = app
        .clone()
        .oneshot(
            Request::get("/api/v1/emojis/remote?q=blob")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let json = read_json(resp).await;
    let items = json["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["shortcode"], "blobcat");

    let resp = app
        .oneshot(
            Request::get("/api/v1/emojis/remote?q=other.example")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let json = read_json(resp).await;
    let items = json["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["shortcode"], "partying");
}

async fn post_copy_from_remote(
    app: axum::Router,
    remote_emoji_id: i64,
    token: &str,
) -> axum::response::Response {
    let body = serde_json::json!({ "remote_emoji_id": remote_emoji_id }).to_string();
    let req = Request::post("/api/v1/emojis/local/from-remote")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap();
    app.oneshot(req).await.unwrap()
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn copy_from_remote_404_when_not_found(pool: PgPool) {
    let token = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = post_copy_from_remote(app, 999_999, &token).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn copy_from_remote_400_when_target_is_local(pool: PgPool) {
    let local_id = insert_local_emoji(&pool, "not_remote").await;
    let token = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = post_copy_from_remote(app, local_id, &token).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn copy_from_remote_409_when_image_key_missing(pool: PgPool) {
    let remote_id = insert_remote_emoji(&pool, "uncached", "misskey.example", None).await;
    let token = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = post_copy_from_remote(app, remote_id, &token).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}
