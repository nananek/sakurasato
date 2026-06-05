//! M14 #160 ── `MiAuth` write endpoints (`notes/create` / `notes/delete` /
//! `notes/renote` / `reactions/create` / `reactions/delete` /
//! `following/create` / `following/delete`) の統合テスト (= 親 issue #150)。
//!
//! `#[sqlx::test]` で per-test DB を切り、`miauth::router` を `tower::ServiceExt::oneshot`
//! で叩く (= `miauth_read_pg.rs` と同形)。
//!
//! ## カバレッジ (= 親 issue #160 Acceptance criteria)
//!
//! - `notes/create { i, text }` で投稿、`createdNote: MissNote` を返す
//! - `notes/delete { i, noteId }` で 204 + `note` 行が削除される
//! - `reactions/create { i, noteId, reaction: "👍" }` で Unicode reaction
//! - `reactions/create { i, noteId, reaction: ":sakura:" }` で local emoji
//! - `reactions/delete { i, noteId }` で自分の reaction を消す
//! - `following/create { i, userId }` で Follow 配送、`delivery_queue` に行が積まれる
//! - `following/delete { i, userId }` で Undo Follow 配送
//! - scope 細分化: `write:reactions` のみの token で `notes/create` が 401
//!
//! ## AGPL discipline
//!
//! 本テスト群は Sakurasato 側 (= MIT) の router/handler/repo を叩くだけ。
//! 実 Misskey との parity は `tests/federation/test_miauth_write_parity.py`
//! (pytest + `misskey-py`) で別経路で確認する。

#![forbid(unsafe_code)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use sakurasato_core::repo;
use sakurasato_core::repo::actor::NewActor;
use sakurasato_core::repo::emoji::NewLocalEmoji;
use sakurasato_core::repo::miauth::NewMiAuthToken;
use sakurasato_server::miauth;
use sakurasato_server::state::AppState;
use serde_json::{Value as JsonValue, json};
use sqlx::PgPool;
use tower::ServiceExt;

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
                max_note_text_length: 3000,
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

async fn seed_remote_actor(pool: &PgPool, host: &str, user: &str) -> i64 {
    let ap_id = format!("https://{host}/users/{user}");
    let new = NewActor {
        ap_id: ap_id.clone(),
        preferred_username: user.into(),
        host: host.into(),
        display_name: Some("Bob".into()),
        summary: Some("remote".into()),
        icon_url: None,
        image_url: None,
        inbox_url: format!("{ap_id}/inbox"),
        shared_inbox_url: Some(format!("https://{host}/inbox")),
        outbox_url: Some(format!("{ap_id}/outbox")),
        followers_url: Some(format!("{ap_id}/followers")),
        following_url: Some(format!("{ap_id}/following")),
        public_key_id: format!("{ap_id}#main-key"),
        public_key_pem: "-----BEGIN PUBLIC KEY-----\nMOCK\n-----END PUBLIC KEY-----".into(),
        private_key_pem: None,
        ed25519_public_key_id: None,
        ed25519_public_key_pem: None,
        ed25519_private_key_pem: None,
        also_known_as: vec![],
        moved_to_ap_id: None,
        is_local: false,
        actor_type: "Person".into(),
        manually_approves_followers: false,
    };
    repo::actor::insert(pool, new)
        .await
        .expect("seed remote actor")
        .id
}

/// remote actor が所有する note を 1 件 seed する (= ownership 拒否テスト用)。
async fn seed_remote_note(pool: &PgPool, actor_id: i64, ap_id: &str) -> i64 {
    repo::note::insert(
        pool,
        repo::note::NewNote {
            ap_id: ap_id.into(),
            actor_id,
            content: "remote post".into(),
            language: None,
            in_reply_to_ap_id: None,
            in_reply_to_note_id: None,
            summary: None,
            visibility: sakurasato_core::model::Visibility::Public,
            sensitive: false,
            to_recipients: vec![],
            cc_recipients: vec![],
            attachments: json!([]),
            tags: json!([]),
            is_local: false,
            url: Some(ap_id.into()),
            published_at: chrono::Utc::now(),
        },
    )
    .await
    .expect("seed remote note")
    .id
}

/// `follower` が `followed` を accepted で follow している状態を作る
/// (= `list_accepted_inboxes(followed)` が follower の inbox を返すようにする)。
#[allow(clippy::similar_names)] // follower_id / followed_id は AP 用語
async fn accepted_follow(pool: &PgPool, follower_id: i64, followed_id: i64) {
    let row = repo::follow::insert_pending(
        pool,
        &format!("https://example.test/follows/{follower_id}-{followed_id}"),
        follower_id,
        followed_id,
    )
    .await
    .expect("insert pending follow");
    repo::follow::set_state(pool, row.id, sakurasato_core::model::FollowState::Accepted)
        .await
        .expect("accept follow");
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

async fn read_json(resp: axum::response::Response) -> JsonValue {
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    if body.is_empty() {
        return JsonValue::Null;
    }
    serde_json::from_slice(&body).expect("response body must be JSON")
}

fn router_for(state: &AppState) -> axum::Router {
    miauth::router(state.clone())
}

fn make_state(pool: PgPool, host: &str, user: &str) -> AppState {
    AppState::from_pool(pool, common::make_config(host, user))
}

async fn post_json(app: axum::Router, path: &str, body: JsonValue) -> axum::response::Response {
    app.oneshot(
        Request::post(path)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap(),
    )
    .await
    .unwrap()
}

// ─── notes/create ──────────────────────────────────────────────────────

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_create_returns_created_note(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:notes"]).await;

    let body = json!({"i": token, "text": "hello from miauth"});
    let resp = post_json(app, "/api/notes/create", body).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_json(resp).await;
    let note = &v["createdNote"];
    assert!(note.is_object(), "createdNote must be object: {v}");
    assert_eq!(note["text"], "hello from miauth");
    assert_eq!(note["user"]["username"], "alice");
    assert!(
        note["user"]["host"].is_null(),
        "local actor user.host must be null"
    );
    // visibility: "public" (default) → "public" (= map_visibility identity)。
    assert_eq!(note["visibility"], "public");
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_create_home_visibility_maps_to_unlisted(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:notes"]).await;

    let body = json!({"i": token, "text": "home only", "visibility": "home"});
    let resp = post_json(app, "/api/notes/create", body).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_json(resp).await;
    // Misskey `home` を internal `unlisted` で保存 → MissNote としては `home` を再出力。
    assert_eq!(v["createdNote"]["visibility"], "home");
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_create_empty_text_returns_400(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:notes"]).await;

    let body = json!({"i": token, "text": "   "});
    let resp = post_json(app, "/api/notes/create", body).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// **PR #166 review item 3 fix**: `fileIds` に数値化できない値が含まれると
/// silent drop せず `400 INVALID_PARAM` を返す (= 添付欠落の無言失敗を防ぐ)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_create_invalid_file_id_returns_400(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:notes"]).await;

    let body = json!({"i": token, "text": "with attachment", "fileIds": ["not-a-number"]});
    let resp = post_json(app, "/api/notes/create", body).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = read_json(resp).await;
    assert_eq!(v["error"]["code"], "INVALID_PARAM");
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_create_without_scope_is_401(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    // **scope 細分化**: write:reactions のみ → notes/create は 401。
    let token = issue_token_with_scopes(&pool, &["write:reactions"]).await;

    let body = json!({"i": token, "text": "x"});
    let resp = post_json(app, "/api/notes/create", body).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ─── notes/delete ──────────────────────────────────────────────────────

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_delete_removes_note_and_returns_204(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:notes"]).await;

    // まず投稿。
    let v = read_json(
        post_json(
            app.clone(),
            "/api/notes/create",
            json!({"i": token, "text": "ephemeral"}),
        )
        .await,
    )
    .await;
    let note_id = v["createdNote"]["id"].as_str().unwrap().to_string();

    // 削除。
    let resp = post_json(
        app,
        "/api/notes/delete",
        json!({"i": token, "noteId": note_id}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // DB から消えている。
    let row = repo::note::get_by_id(&pool, note_id.parse::<i64>().unwrap())
        .await
        .unwrap();
    assert!(row.is_none(), "note row must be deleted");
}

/// **PR #166 review item 5(a)**: remote actor が所有する note を local user の
/// token で削除しようとしても `403 PERMISSION_DENIED` で弾き、note 行は残す
/// (= ownership 検査が note を消す前に効く)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_delete_remote_owned_note_returns_403(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let remote_id = seed_remote_actor(&pool, "misskey.io", "bob").await;
    let note_id = seed_remote_note(&pool, remote_id, "https://misskey.io/notes/abc").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:notes"]).await;

    let resp = post_json(
        app,
        "/api/notes/delete",
        json!({"i": token, "noteId": note_id.to_string()}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let v = read_json(resp).await;
    assert_eq!(v["error"]["code"], "PERMISSION_DENIED");
    // ownership 検査 (`note.actor_id != viewer`) が先に効くことを message で pin
    // する (= `!note.is_local` 分岐ではなく所有権分岐が短絡している証拠)。
    assert_eq!(v["error"]["message"], "note not owned by you");

    // 他人の note を消していないこと。
    let row = repo::note::get_by_id(&pool, note_id).await.unwrap();
    assert!(row.is_some(), "remote note must not be deleted");
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_delete_unknown_note_returns_404(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:notes"]).await;

    let resp = post_json(
        app,
        "/api/notes/delete",
        json!({"i": token, "noteId": "999999"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// **PR #166 review item 2 の test**: delete 配送される Delete activity の id が
/// `{note_ap_id}/activity/delete-{note.id}` で **決定論的** (= ms timestamp 依存
/// ではない) ことを `delivery_queue` 経由で検証する。accepted follower を 1 人作って
/// 配送先 inbox を確保しないと `delivery_queue` が空になるので bob を follow させる。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_delete_enqueues_deterministic_delete_activity(pool: PgPool) {
    let alice = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let bob = seed_remote_actor(&pool, "misskey.io", "bob").await;
    // bob → alice の accepted follow (= alice の note は bob の inbox に配送される)。
    accepted_follow(&pool, bob, alice).await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:notes"]).await;

    // 投稿 → 削除。
    let v = read_json(
        post_json(
            app.clone(),
            "/api/notes/create",
            json!({"i": token, "text": "delete me"}),
        )
        .await,
    )
    .await;
    let note_id = v["createdNote"]["id"].as_str().unwrap().to_string();

    let resp = post_json(
        app,
        "/api/notes/delete",
        json!({"i": token, "noteId": note_id}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // delivery_queue に Delete activity が積まれ、その id が決定論的であること。
    // runtime クエリ (= マクロでない) なので .sqlx offline cache は不要。
    let activity: JsonValue = sqlx::query_scalar(
        "SELECT activity FROM delivery_queue WHERE activity->>'type' = 'Delete' ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .expect("a Delete activity must be enqueued to the follower inbox");

    assert_eq!(activity["type"], "Delete");
    let activity_id = activity["id"].as_str().expect("activity id must be string");
    assert!(
        activity_id.ends_with(&format!("/activity/delete-{note_id}")),
        "delete activity id must be deterministic note-anchored (delete-<note.id>), got {activity_id}"
    );
    // object は削除対象 note の ap_id。
    let object = activity["object"].as_str().expect("object must be string");
    assert!(
        object.ends_with(&format!("/notes/{note_id}")),
        "Delete object must reference the note ap_id, got {object}"
    );
}

// ─── reactions/create + delete ─────────────────────────────────────────

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn reactions_create_unicode_returns_204(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let write_token = issue_token_with_scopes(&pool, &["write:notes"]).await;
    let react_token = issue_token_with_scopes(&pool, &["write:reactions"]).await;

    // 投稿。
    let v = read_json(
        post_json(
            app.clone(),
            "/api/notes/create",
            json!({"i": write_token, "text": "reactme"}),
        )
        .await,
    )
    .await;
    let note_id = v["createdNote"]["id"].as_str().unwrap().to_string();

    // Unicode reaction。
    let resp = post_json(
        app,
        "/api/notes/reactions/create",
        json!({"i": react_token, "noteId": note_id, "reaction": "👍"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let counts = repo::reaction::count_by_note(&pool, note_id.parse::<i64>().unwrap())
        .await
        .unwrap();
    assert_eq!(counts.len(), 1);
    assert_eq!(counts[0].content, "👍");
    assert_eq!(counts[0].count, 1);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn reactions_create_local_emoji_returns_204(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let _ = repo::emoji::upsert_local(
        &pool,
        NewLocalEmoji {
            shortcode: "sakura".into(),
            category: None,
            aliases: vec![],
            image_key: "emoji/local/sakura.webp".into(),
            media_type: "image/webp".into(),
        },
    )
    .await
    .expect("seed emoji");
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let write_token = issue_token_with_scopes(&pool, &["write:notes"]).await;
    let react_token = issue_token_with_scopes(&pool, &["write:reactions"]).await;

    let v = read_json(
        post_json(
            app.clone(),
            "/api/notes/create",
            json!({"i": write_token, "text": "ohayo"}),
        )
        .await,
    )
    .await;
    let note_id = v["createdNote"]["id"].as_str().unwrap().to_string();

    let resp = post_json(
        app,
        "/api/notes/reactions/create",
        json!({"i": react_token, "noteId": note_id, "reaction": ":sakura:"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn reactions_create_unknown_local_emoji_returns_404(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let write_token = issue_token_with_scopes(&pool, &["write:notes"]).await;
    let react_token = issue_token_with_scopes(&pool, &["write:reactions"]).await;

    let v = read_json(
        post_json(
            app.clone(),
            "/api/notes/create",
            json!({"i": write_token, "text": "x"}),
        )
        .await,
    )
    .await;
    let note_id = v["createdNote"]["id"].as_str().unwrap().to_string();

    let resp = post_json(
        app,
        "/api/notes/reactions/create",
        json!({"i": react_token, "noteId": note_id, "reaction": ":missing_emoji:"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let v = read_json(resp).await;
    assert_eq!(v["error"]["code"], "NO_SUCH_EMOJI");
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn reactions_delete_removes_my_reaction_on_note(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let write_token = issue_token_with_scopes(&pool, &["write:notes"]).await;
    let react_token = issue_token_with_scopes(&pool, &["write:reactions"]).await;

    let v = read_json(
        post_json(
            app.clone(),
            "/api/notes/create",
            json!({"i": write_token, "text": "go"}),
        )
        .await,
    )
    .await;
    let note_id = v["createdNote"]["id"].as_str().unwrap().to_string();

    // Add reaction first.
    let _ = post_json(
        app.clone(),
        "/api/notes/reactions/create",
        json!({"i": react_token.clone(), "noteId": note_id, "reaction": "👍"}),
    )
    .await;

    // Now delete.
    let resp = post_json(
        app,
        "/api/notes/reactions/delete",
        json!({"i": react_token, "noteId": note_id}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let counts = repo::reaction::count_by_note(&pool, note_id.parse::<i64>().unwrap())
        .await
        .unwrap();
    assert!(counts.is_empty(), "reaction row must be deleted");
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn reactions_create_without_scope_is_401(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    // scope 細分化: write:notes のみ → reactions/create は 401。
    let token = issue_token_with_scopes(&pool, &["write:notes"]).await;

    let resp = post_json(
        app,
        "/api/notes/reactions/create",
        json!({"i": token, "noteId": "1", "reaction": "👍"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ─── following/create + delete ─────────────────────────────────────────

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn following_create_enqueues_follow(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let target_id = seed_remote_actor(&pool, "misskey.io", "bob").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:following"]).await;

    let body = json!({"i": token, "userId": target_id.to_string()});
    let resp = post_json(app, "/api/following/create", body).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_json(resp).await;
    // 相手 user の UserDetailed が返る。
    assert_eq!(v["username"], "bob");
    assert_eq!(v["host"], "misskey.io");

    // delivery_queue に Follow 行が積まれている。
    let queued: i64 = sqlx::query_scalar!("SELECT count(*) FROM delivery_queue")
        .fetch_one(&pool)
        .await
        .unwrap()
        .unwrap_or(0);
    assert!(queued >= 1, "delivery_queue should have at least 1 row");
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn following_delete_enqueues_undo(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let target_id = seed_remote_actor(&pool, "misskey.io", "bob").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:following"]).await;

    // Follow first.
    let _ = post_json(
        app.clone(),
        "/api/following/create",
        json!({"i": token.clone(), "userId": target_id.to_string()}),
    )
    .await;

    // Then unfollow.
    let resp = post_json(
        app,
        "/api/following/delete",
        json!({"i": token, "userId": target_id.to_string()}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_json(resp).await;
    assert_eq!(v["username"], "bob");

    // delivery_queue に Follow + Undo の 2 行が積まれている。
    let queued: i64 = sqlx::query_scalar!("SELECT count(*) FROM delivery_queue")
        .fetch_one(&pool)
        .await
        .unwrap()
        .unwrap_or(0);
    assert!(
        queued >= 2,
        "delivery_queue should have follow + undo (got {queued})"
    );
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn following_delete_not_following_returns_404(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let target_id = seed_remote_actor(&pool, "misskey.io", "bob").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:following"]).await;

    let resp = post_json(
        app,
        "/api/following/delete",
        json!({"i": token, "userId": target_id.to_string()}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let v = read_json(resp).await;
    assert_eq!(v["error"]["code"], "NOT_FOLLOWING");
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn following_create_self_returns_conflict(pool: PgPool) {
    let local_id = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:following"]).await;

    let resp = post_json(
        app,
        "/api/following/create",
        json!({"i": token, "userId": local_id.to_string()}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let v = read_json(resp).await;
    assert_eq!(v["error"]["code"], "ALREADY_FOLLOWING");
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn following_create_without_scope_is_401(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let target_id = seed_remote_actor(&pool, "misskey.io", "bob").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    // scope 細分化: write:notes のみ → following/create は 401。
    let token = issue_token_with_scopes(&pool, &["write:notes"]).await;

    let resp = post_json(
        app,
        "/api/following/create",
        json!({"i": token, "userId": target_id.to_string()}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ─── notes/renote (Iceshrimp alias) ────────────────────────────────────

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_renote_returns_501(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:notes"]).await;

    // renoteId のみ (= 本 PR では未対応で 501)。
    let body = json!({"i": token, "renoteId": "1"});
    let resp = post_json(app, "/api/notes/renote", body).await;
    assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
}

/// `replyId` 指定の `notes/create` が親 note への返信として成立する
/// (= #166 で deferred だった reply 実装、Aria 実機検証で必要と判明)。
/// `createdNote.replyId` が親の id を指す。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_create_reply_links_parent(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:notes"]).await;

    // 親 note を投稿。
    let parent = read_json(
        post_json(
            app.clone(),
            "/api/notes/create",
            json!({"i": token, "text": "parent note"}),
        )
        .await,
    )
    .await;
    let parent_id = parent["createdNote"]["id"].as_str().unwrap().to_string();

    // その note に自己返信。
    let resp = post_json(
        app,
        "/api/notes/create",
        json!({"i": token, "text": "a reply", "replyId": parent_id}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_json(resp).await;
    assert_eq!(v["createdNote"]["text"], "a reply");
    // 返信関係が張られている (= replyId が親 id を指す)。
    assert_eq!(
        v["createdNote"]["replyId"], parent_id,
        "createdNote.replyId must point at the parent note"
    );
}

/// `replyId` が存在しない note を指すと `400 NO_SUCH_REPLY_TARGET` (= silent に
/// 単独 note 化せず明示エラー)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_create_reply_to_unknown_returns_error(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:notes"]).await;

    let resp = post_json(
        app,
        "/api/notes/create",
        json!({"i": token, "text": "reply to ghost", "replyId": "999999"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = read_json(resp).await;
    assert_eq!(v["error"]["code"], "NO_SUCH_REPLY_TARGET");
}
