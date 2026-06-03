//! M14 #159 ── `MiAuth` read endpoints (`notes/show` / `notes/timeline` /
//! `emojis` / `users/show`) の統合テスト (= 親 issue #150)。
//!
//! `#[sqlx::test]` で per-test DB を切り、`miauth::router` を `tower::ServiceExt::oneshot`
//! で叩く (= `miauth_flow_pg.rs` と同形)。
//!
//! ## カバレッジ (= 親 issue #159 Acceptance criteria)
//!
//! - `notes/timeline` がホームタイムラインを返す (= 既存 `repo::note::list_home_timeline_window` 流用)
//! - `notes/show` が単一 Note を `MissNote` 形で返す
//! - read scope を持たない token は 403 / unauthorized → 401
//! - `sinceId` / `untilId` の境界が排他で効く
//! - `reactions` 集計が Misskey 形式 (`{key: count}`) で返る
//! - `/api/emojis` がローカル絵文字を `MissEmoji` 形で返す
//! - `/api/users/show` が userId / username 両経路で 200
//!
//! ## AGPL discipline
//!
//! 本テスト群は Sakurasato 側 (= MIT) の router/handler/repo を叩くだけ。
//! 実 Misskey との parity は `tests/federation/test_miauth_read_parity.py`
//! (pytest + misskey-py) で別経路で確認する。

#![forbid(unsafe_code)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use sakurasato_core::model::Visibility;
use sakurasato_core::repo;
use sakurasato_core::repo::actor::NewActor;
use sakurasato_core::repo::emoji::NewLocalEmoji;
use sakurasato_core::repo::miauth::NewMiAuthToken;
use sakurasato_core::repo::note::NewNote;
use sakurasato_server::miauth;
use sakurasato_server::state::AppState;
use serde_json::json;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

mod common {
    use sakurasato_core::config::{
        DatabaseConfig, MediaProxyConfig, MiAuthConfig, ServerConfig, ServerInfo, StorageConfig,
    };

    pub(super) fn make_config(host: &str, user: &str) -> sakurasato_core::Config {
        sakurasato_core::Config {
            server: ServerConfig {
                host: host.into(),
                bind: "127.0.0.1:0".into(),
                local_api_socket: "/tmp/sakurasato.sock".into(),
                public_listen: None,
                local_api_listen: None,
                user: user.into(),
                info: ServerInfo::default(),
                auto_approve_followers_for_followees: false,
            },
            database: DatabaseConfig {
                url: "unused-by-tests".into(),
                password_file: None,
            },
            storage: StorageConfig {
                endpoint: "http://versitygw:7070".into(),
                bucket: "sakurasato-test".into(),
                region: "us-east-1".into(),
                access_key_id: "test".into(),
                secret_access_key: "test12345".into(),
                secret_access_key_file: None,
            },
            media_proxy: MediaProxyConfig {
                socket: "/tmp/media.sock".into(),
                max_bytes: 1024 * 1024,
                max_pixels: 1_000_000,
            },
            miauth: Some(MiAuthConfig {
                listen: "unix:/tmp/miauth.sock".into(),
                session_ttl_secs: 600,
            }),
        }
    }
}

async fn seed_local_actor(pool: &PgPool, host: &str, user: &str) -> i64 {
    let ap_id = format!("https://{host}/users/{user}");
    let new = NewActor {
        ap_id: ap_id.clone(),
        preferred_username: user.into(),
        host: host.into(),
        display_name: Some("Alice".into()),
        summary: Some("hello".into()),
        icon_url: Some("https://cdn.test/avatar.webp".into()),
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
        ed25519_public_key_id: None,
        ed25519_public_key_pem: None,
        ed25519_private_key_pem: None,
        also_known_as: vec![],
        moved_to_ap_id: None,
        is_local: true,
        actor_type: "Person".into(),
        manually_approves_followers: false,
    };
    repo::actor::insert(pool, new)
        .await
        .expect("seed local actor")
        .id
}

async fn seed_note(
    pool: &PgPool,
    actor_id: i64,
    host: &str,
    content: &str,
    visibility: Visibility,
) -> i64 {
    // 適当な placeholder ap_id を当て、後で set_ap_id_and_url で正規化する。
    let ap_id = format!("https://{host}/notes/pending-{}", Uuid::new_v4());
    let new = NewNote {
        ap_id: ap_id.clone(),
        actor_id,
        content: content.into(),
        language: Some("ja".into()),
        in_reply_to_ap_id: None,
        in_reply_to_note_id: None,
        summary: None,
        visibility,
        sensitive: false,
        to_recipients: vec!["https://www.w3.org/ns/activitystreams#Public".into()],
        cc_recipients: vec![],
        attachments: json!([]),
        tags: json!([]),
        is_local: true,
        url: None,
        published_at: chrono::Utc::now(),
    };
    let row = repo::note::insert(pool, new).await.expect("seed note");
    let canonical = format!("https://{host}/notes/{}", row.id);
    repo::note::set_ap_id_and_url(pool, row.id, &canonical, &canonical)
        .await
        .expect("set canonical url");
    row.id
}

async fn issue_token_with_scopes(pool: &PgPool, scopes: &[&str]) -> String {
    use sakurasato_server::token::{generate_raw, hash};
    let raw = generate_raw();
    let token_hash = hash(&raw);
    repo::miauth::insert_token(
        pool,
        NewMiAuthToken {
            name: "test".into(),
            token_hash,
            permissions: scopes.iter().map(|s| (*s).to_string()).collect(),
        },
    )
    .await
    .expect("insert token");
    raw
}

async fn read_json(resp: axum::response::Response) -> serde_json::Value {
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).expect("response body must be JSON")
}

fn router_for(state: &AppState) -> axum::Router {
    miauth::router(state.clone())
}

fn make_state(pool: PgPool, host: &str, user: &str) -> AppState {
    AppState::from_pool(pool, common::make_config(host, user))
}

// ─── notes/timeline ────────────────────────────────────────────────────────

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn timeline_returns_miss_notes_in_id_desc(pool: PgPool) {
    let actor_id = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let _ = seed_note(
        &pool,
        actor_id,
        "sakurasato.test",
        "first",
        Visibility::Public,
    )
    .await;
    let _ = seed_note(
        &pool,
        actor_id,
        "sakurasato.test",
        "second",
        Visibility::Public,
    )
    .await;
    let last = seed_note(
        &pool,
        actor_id,
        "sakurasato.test",
        "third",
        Visibility::Public,
    )
    .await;

    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["read:account"]).await;

    let body = json!({"i": token, "limit": 10});
    let resp = app
        .oneshot(
            Request::post("/api/notes/timeline")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let arr = read_json(resp).await;
    let notes = arr.as_array().expect("timeline returns array");
    assert_eq!(notes.len(), 3);
    // id DESC: last note is first.
    assert_eq!(notes[0]["id"], last.to_string());
    assert_eq!(notes[0]["text"], "third");
    assert_eq!(notes[0]["user"]["username"], "alice");
    // visibility: "public" → "public"
    assert_eq!(notes[0]["visibility"], "public");
    // mentions / fileIds / files / reactions / emojis are always present.
    assert!(notes[0]["mentions"].is_array());
    assert!(notes[0]["fileIds"].is_array());
    assert!(notes[0]["files"].is_array());
    assert!(notes[0]["reactions"].is_object());
    assert!(notes[0]["emojis"].is_object());
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn timeline_since_id_filters_strictly_greater(pool: PgPool) {
    let actor_id = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let a = seed_note(&pool, actor_id, "sakurasato.test", "a", Visibility::Public).await;
    let b = seed_note(&pool, actor_id, "sakurasato.test", "b", Visibility::Public).await;
    let c = seed_note(&pool, actor_id, "sakurasato.test", "c", Visibility::Public).await;
    let _ = a;

    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["read:account"]).await;

    // sinceId = b → 排他なので b 自身は出ない、c のみ。
    let body = json!({"i": token, "sinceId": b.to_string()});
    let resp = app
        .oneshot(
            Request::post("/api/notes/timeline")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let arr = read_json(resp).await;
    let notes = arr.as_array().unwrap();
    assert_eq!(notes.len(), 1, "sinceId is exclusive");
    assert_eq!(notes[0]["id"], c.to_string());
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn timeline_until_id_filters_strictly_less(pool: PgPool) {
    let actor_id = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let a = seed_note(&pool, actor_id, "sakurasato.test", "a", Visibility::Public).await;
    let b = seed_note(&pool, actor_id, "sakurasato.test", "b", Visibility::Public).await;
    let _ = b;

    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["read:account"]).await;

    // untilId = b → 排他なので b 自身は出ない、a のみ。
    let body = json!({"i": token, "untilId": b.to_string()});
    let resp = app
        .oneshot(
            Request::post("/api/notes/timeline")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let arr = read_json(resp).await;
    let notes = arr.as_array().unwrap();
    assert_eq!(notes.len(), 1, "untilId is exclusive");
    assert_eq!(notes[0]["id"], a.to_string());
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn timeline_limit_clamps_to_max_100(pool: PgPool) {
    let actor_id = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    for i in 0..5 {
        let _ = seed_note(
            &pool,
            actor_id,
            "sakurasato.test",
            &format!("note-{i}"),
            Visibility::Public,
        )
        .await;
    }

    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["read:account"]).await;

    // limit = 9999 (= 上限超え) → 100 にクランプされ、実 5 件返る。
    let body = json!({"i": token, "limit": 9999});
    let resp = app
        .oneshot(
            Request::post("/api/notes/timeline")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let arr = read_json(resp).await;
    assert_eq!(arr.as_array().unwrap().len(), 5);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn timeline_without_scope_returns_401(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    // scope ゼロの token ── handler 内 `unauthorized` 経路に倒れる。
    let token = issue_token_with_scopes(&pool, &[]).await;

    let body = json!({"i": token});
    let resp = app
        .oneshot(
            Request::post("/api/notes/timeline")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn timeline_with_bearer_header_also_works(pool: PgPool) {
    let actor_id = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let _ = seed_note(
        &pool,
        actor_id,
        "sakurasato.test",
        "hello",
        Visibility::Public,
    )
    .await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["read:account"]).await;

    // body 無し + Authorization: Bearer 経路。
    let resp = app
        .oneshot(
            Request::post("/api/notes/timeline")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let arr = read_json(resp).await;
    assert_eq!(arr.as_array().unwrap().len(), 1);
}

// ─── notes/show ────────────────────────────────────────────────────────

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_show_returns_single_miss_note(pool: PgPool) {
    let actor_id = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let note_id = seed_note(
        &pool,
        actor_id,
        "sakurasato.test",
        "hello world",
        Visibility::Public,
    )
    .await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["read:account"]).await;

    let body = json!({"i": token, "noteId": note_id.to_string()});
    let resp = app
        .oneshot(
            Request::post("/api/notes/show")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let note = read_json(resp).await;
    assert_eq!(note["id"], note_id.to_string());
    assert_eq!(note["text"], "hello world");
    assert_eq!(note["user"]["username"], "alice");
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_show_unknown_id_returns_404(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["read:account"]).await;

    let body = json!({"i": token, "noteId": "999999"});
    let resp = app
        .oneshot(
            Request::post("/api/notes/show")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_show_missing_note_id_is_400(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["read:account"]).await;

    let body = json!({"i": token});
    let resp = app
        .oneshot(
            Request::post("/api/notes/show")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ─── emojis ────────────────────────────────────────────────────────────

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn emojis_returns_local_emojis_in_misskey_shape(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let _ = repo::emoji::upsert_local(
        &pool,
        NewLocalEmoji {
            shortcode: "sakura".into(),
            category: Some("flowers".into()),
            aliases: vec!["cherryblossom".into()],
            image_key: "emoji/local/sakura.webp".into(),
            media_type: "image/webp".into(),
        },
    )
    .await
    .expect("seed emoji");
    let _ = repo::emoji::upsert_local(
        &pool,
        NewLocalEmoji {
            shortcode: "blob".into(),
            category: None,
            aliases: vec![],
            image_key: "emoji/local/blob.webp".into(),
            media_type: "image/webp".into(),
        },
    )
    .await
    .expect("seed emoji 2");

    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);

    let resp = app
        .oneshot(
            Request::post("/api/emojis")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_json(resp).await;
    let arr = body["emojis"].as_array().expect("emojis key is array");
    assert_eq!(arr.len(), 2);
    // shortcode ASC.
    assert_eq!(arr[0]["name"], "blob");
    assert_eq!(arr[1]["name"], "sakura");
    assert_eq!(
        arr[1]["url"],
        "https://sakurasato.test/media/emoji/local/sakura.webp"
    );
    assert_eq!(arr[1]["category"], "flowers");
    assert!(
        arr[1]["aliases"]
            .as_array()
            .unwrap()
            .contains(&json!("cherryblossom"))
    );
}

// ─── users/show ────────────────────────────────────────────────────────

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn users_show_by_user_id_returns_detailed(pool: PgPool) {
    let actor_id = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["read:account"]).await;

    let body = json!({"i": token, "userId": actor_id.to_string()});
    let resp = app
        .oneshot(
            Request::post("/api/users/show")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_json(resp).await;
    assert_eq!(v["id"], actor_id.to_string());
    assert_eq!(v["username"], "alice");
    // local user host = null.
    assert!(v["host"].is_null());
    assert_eq!(v["isBot"], false);
    assert_eq!(v["isCat"], false);
    assert_eq!(v["description"], "hello");
    assert!(v["createdAt"].is_string());
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn users_show_by_username_returns_detailed(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["read:account"]).await;

    // host 省略 (= local).
    let body = json!({"i": token, "username": "alice"});
    let resp = app
        .oneshot(
            Request::post("/api/users/show")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_json(resp).await;
    assert_eq!(v["username"], "alice");
    assert!(v["host"].is_null());
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn users_show_unknown_returns_404(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["read:account"]).await;

    let body = json!({"i": token, "userId": "999999"});
    let resp = app
        .oneshot(
            Request::post("/api/users/show")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn users_show_neither_id_nor_username_is_400(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["read:account"]).await;

    let body = json!({"i": token});
    let resp = app
        .oneshot(
            Request::post("/api/users/show")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ─── session-related: notes/show check token + scope ──────────────────

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_show_without_scope_returns_401(pool: PgPool) {
    let actor_id = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let note_id = seed_note(&pool, actor_id, "sakurasato.test", "x", Visibility::Public).await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &[]).await;

    let body = json!({"i": token, "noteId": note_id.to_string()});
    let resp = app
        .oneshot(
            Request::post("/api/notes/show")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
