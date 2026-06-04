//! Issue #130 統合テスト: `GET /api/v1/emojis` の部分一致検索 (`q=`) と
//! 100 件超のフェッチ上限。
//!
//! - 部分一致 (`q`) が shortcode / aliases にかかる
//! - `limit=10000` で 100 件超を返せる (= 旧 `MAX_LIMIT = 100` を撤廃)
//! - `q` 未指定なら従来 `prefix` 経路を保持 (= 既存挙動の回帰なし)
//! - 過大な `limit` は `MAX_LIMIT` にクランプ
//! - LIKE メタ文字 (`%` / `_`) はエスケープされ、全件マッチに化けない

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
        },
        media_proxy: sakurasato_core::config::MediaProxyConfig {
            socket: "/tmp/x".into(),
            max_bytes: 1024,
            max_pixels: 1024,
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

async fn insert_local_emoji(pool: &PgPool, shortcode: &str, aliases: Vec<String>) {
    repo::emoji::upsert_local(
        pool,
        repo::emoji::NewLocalEmoji {
            shortcode: shortcode.into(),
            category: None,
            aliases,
            image_key: format!("emoji/local/{shortcode}.webp"),
            media_type: "image/webp".into(),
        },
    )
    .await
    .unwrap();
}

async fn get_emojis(app: axum::Router, query: &str, token: &str) -> axum::response::Response {
    app.oneshot(
        Request::get(format!("/api/v1/emojis?{query}"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .unwrap()
}

fn shortcodes(json: &serde_json::Value) -> Vec<String> {
    json["items"]
        .as_array()
        .expect("items array")
        .iter()
        .map(|item| item["shortcode"].as_str().unwrap().to_string())
        .collect()
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn substring_query_matches_inside_shortcode(pool: PgPool) {
    insert_local_emoji(&pool, "bonfire", vec![]).await;
    insert_local_emoji(&pool, "firework", vec![]).await;
    insert_local_emoji(&pool, "sad", vec![]).await;
    let token = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = get_emojis(app, "q=fire", &token).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    let mut codes = shortcodes(&json);
    codes.sort();
    assert_eq!(codes, vec!["bonfire", "firework"]);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn substring_query_matches_inside_aliases(pool: PgPool) {
    insert_local_emoji(
        &pool,
        "partying-face",
        vec!["celebrate".into(), "yay".into()],
    )
    .await;
    insert_local_emoji(&pool, "sad", vec![]).await;
    let token = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = get_emojis(app, "q=celebr", &token).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let codes = shortcodes(&read_json(resp).await);
    assert_eq!(codes, vec!["partying-face"]);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn substring_query_is_case_insensitive(pool: PgPool) {
    insert_local_emoji(&pool, "Happy_Cat", vec![]).await;
    insert_local_emoji(&pool, "sad", vec![]).await;
    let token = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = get_emojis(app, "q=HAPPY", &token).await;
    let codes = shortcodes(&read_json(resp).await);
    assert_eq!(codes, vec!["Happy_Cat"]);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn substring_query_empty_falls_back_to_prefix_behavior(pool: PgPool) {
    // q="" は無視されて prefix 経路 (= 既存挙動) を踏む。prefix 未指定なら全件。
    insert_local_emoji(&pool, "alpha", vec![]).await;
    insert_local_emoji(&pool, "beta", vec![]).await;
    let token = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = get_emojis(app, "q=", &token).await;
    let mut codes = shortcodes(&read_json(resp).await);
    codes.sort();
    assert_eq!(codes, vec!["alpha", "beta"]);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn prefix_path_unchanged_when_q_absent(pool: PgPool) {
    // 旧クライアントが `prefix=foo&limit=20` で来た場合の互換性。
    insert_local_emoji(&pool, "foo_one", vec![]).await;
    insert_local_emoji(&pool, "foo_two", vec![]).await;
    insert_local_emoji(&pool, "bar", vec![]).await;
    let token = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = get_emojis(app, "prefix=foo", &token).await;
    let mut codes = shortcodes(&read_json(resp).await);
    codes.sort();
    assert_eq!(codes, vec!["foo_one", "foo_two"]);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn limit_10000_returns_more_than_100_rows(pool: PgPool) {
    // Issue #130 の本丸: 100 件超のローカル絵文字を全部取れる。
    // 200 件は upsert_local が逐次 await のため初回構築に若干時間がかかるが、
    // pytest 同等の数百 ms 級で済む想定。
    for i in 0..150_u32 {
        let code = format!("e_{i:04}");
        insert_local_emoji(&pool, &code, vec![]).await;
    }
    let token = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = get_emojis(app, "limit=10000", &token).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let codes = shortcodes(&read_json(resp).await);
    assert_eq!(codes.len(), 150, "all 150 rows must be returned");
    assert_eq!(codes.first().map(String::as_str), Some("e_0000"));
    assert_eq!(codes.last().map(String::as_str), Some("e_0149"));
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn limit_above_max_is_clamped(pool: PgPool) {
    // `limit=99999` は `MAX_LIMIT = 10000` にクランプされ、結果は最大 10000 件
    // で打ち止め。本テストは 3 件しか入れないので 200 OK + 3 件返るのみ。
    insert_local_emoji(&pool, "alpha", vec![]).await;
    insert_local_emoji(&pool, "beta", vec![]).await;
    insert_local_emoji(&pool, "gamma", vec![]).await;
    let token = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = get_emojis(app, "limit=99999", &token).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let mut codes = shortcodes(&read_json(resp).await);
    codes.sort();
    assert_eq!(codes, vec!["alpha", "beta", "gamma"]);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn like_metacharacters_in_query_are_escaped(pool: PgPool) {
    // shortcode は `[A-Za-z0-9_-]{1,128}` (Issue #188) なので shortcode 直接には `%` は入らない
    // が、`_` は valid 文字。`q=_` を投げたとき *全件* マッチに化けないことを
    // 保証する (= `_` は SQL LIKE で「任意 1 文字」だが、エスケープして「文字
    // としての `_`」だけに絞れていること)。
    insert_local_emoji(&pool, "alpha", vec![]).await;
    insert_local_emoji(&pool, "beta", vec![]).await;
    insert_local_emoji(&pool, "with_underscore", vec![]).await;
    let token = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = get_emojis(app, "q=_", &token).await;
    let codes = shortcodes(&read_json(resp).await);
    // `_` は escape されるので `with_underscore` だけがヒットする。
    assert_eq!(codes, vec!["with_underscore"]);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn q_takes_precedence_over_prefix(pool: PgPool) {
    // `prefix=foo&q=bar` のとき q が勝つ仕様。両方指定された場合の優先順位を
    // ハンドラの doc コメントどおり q 優先で固定する。
    insert_local_emoji(&pool, "foo_thing", vec![]).await;
    insert_local_emoji(&pool, "bar_thing", vec![]).await;
    let token = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = get_emojis(app, "prefix=foo&q=bar", &token).await;
    let codes = shortcodes(&read_json(resp).await);
    assert_eq!(codes, vec!["bar_thing"]);
}
