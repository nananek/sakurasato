//! `GET /emojis/{shortcode}` (FEP-9098 AP `Emoji` object) の統合テスト。
//! 我々が inline tag で発行する `Emoji.id` を dereferenceable にした経路。

#![forbid(unsafe_code)]

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
            socket: "/tmp/x".into(),
            max_bytes: 1024,
            max_pixels: 1024,
        },
        miauth: None,
    }
}

async fn read_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn seed_local_emoji(pool: &PgPool, shortcode: &str) {
    repo::emoji::upsert_local(
        pool,
        repo::emoji::NewLocalEmoji {
            shortcode: shortcode.into(),
            category: Some("blob".into()),
            aliases: vec!["cat".into()],
            image_key: format!("emoji/local/{shortcode}.webp"),
            media_type: "image/webp".into(),
        },
    )
    .await
    .unwrap();
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn emoji_object_200_shape(pool: PgPool) {
    seed_local_emoji(&pool, "blobcat").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::routes::router(state);
    let resp = app
        .oneshot(Request::get("/emojis/blobcat").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp.headers().get(header::CONTENT_TYPE).unwrap();
    assert_eq!(ct, "application/activity+json");
    let json = read_json(resp).await;
    assert_eq!(json["type"], "Emoji");
    assert_eq!(json["id"], "https://example.test/emojis/blobcat");
    assert_eq!(json["name"], ":blobcat:");
    assert_eq!(json["icon"]["type"], "Image");
    assert_eq!(json["icon"]["mediaType"], "image/webp");
    assert_eq!(
        json["icon"]["url"],
        "https://example.test/media/emoji/local/blobcat.webp"
    );
    assert!(json["updated"].is_string());
    assert_eq!(json["@context"][0], "https://www.w3.org/ns/activitystreams");
    assert_eq!(json["@context"][1]["Emoji"], "toot:Emoji");
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn emoji_object_404_for_unknown(pool: PgPool) {
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::routes::router(state);
    let resp = app
        .oneshot(Request::get("/emojis/nope").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn emoji_object_404_for_null_image_key(pool: PgPool) {
    seed_local_emoji(&pool, "ghost").await;
    sqlx::query("UPDATE emoji SET image_key = NULL WHERE shortcode = $1 AND host IS NULL")
        .bind("ghost")
        .execute(&pool)
        .await
        .unwrap();
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::routes::router(state);
    let resp = app
        .oneshot(Request::get("/emojis/ghost").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn emoji_object_404_for_remote(pool: PgPool) {
    // remote emoji は get_local_by_shortcode (host IS NULL) で拾われない → 404。
    repo::emoji::upsert_remote(
        &pool,
        repo::emoji::NewRemoteEmoji {
            shortcode: "blobcat".into(),
            ap_id: "https://remote.test/emojis/blobcat".into(),
            host: "remote.test".into(),
            image_key: Some("emoji/remote/remote.test/blobcat.webp".into()),
            media_type: "image/webp".into(),
            last_failed_at: None,
        },
    )
    .await
    .unwrap();
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::routes::router(state);
    let resp = app
        .oneshot(Request::get("/emojis/blobcat").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
