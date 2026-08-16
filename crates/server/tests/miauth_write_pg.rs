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
//! - `following/requests/{list,accept,reject,cancel}` で鍵アカ承認待ちの確認・
//!   承認・拒否・取り下げ (cancel は送信側 pending の Undo Follow 配送)
//! - `i/update { i, description, avatarId, bannerId, isLocked, birthday, ... }`
//!   でプロフィール編集 (Aria `INotifier` の crash fix)、`MeDetailed` を返し
//!   `Update` activity をフォロワーに配送する
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
                public_base_url: None,
            },
            media_proxy: MediaProxyConfig {
                socket: "/tmp/media.sock".into(),
                max_bytes: 1024 * 1024,
                max_pixels: 1_000_000,
                video: sakurasato_core::config::VideoConfig::default(),
                emoji_import: sakurasato_core::config::EmojiImportConfig::default(),
            },
            miauth: Some(MiAuthConfig {
                listen: "unix:/tmp/miauth.sock".into(),
                session_ttl_secs: 600,
                // 厳密 scope 検証のテストは `ignore_scope = false` で走らせ、
                // アナーキー (ON) のテストだけ個別に `true` を入れる。
                ignore_scope: false,
            }),
        }
    }

    /// `make_config` のアナーキー版 ── `ignore_scope = true` (= お一人様 default)。
    /// アナーキー系テスト (`anarchy_*`) 専用。
    pub(super) fn make_config_anarchy(host: &str, user: &str) -> sakurasato_core::Config {
        let mut cfg = make_config(host, user);
        cfg.miauth.as_mut().unwrap().ignore_scope = true;
        cfg
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

/// `follower` から `followed` への Follow を `pending` のまま作る (= 鍵アカ
/// 運用の承認待ち状態を再現する)。`accepted_follow` と対で
/// `following/requests/*` テスト用。
#[allow(clippy::similar_names)] // follower_id / followed_id は AP 用語
async fn pending_follow(pool: &PgPool, follower_id: i64, followed_id: i64) -> i64 {
    repo::follow::insert_pending(
        pool,
        &format!("https://example.test/follows/pending-{follower_id}-{followed_id}"),
        follower_id,
        followed_id,
    )
    .await
    .expect("insert pending follow")
    .id
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
async fn notes_create_without_scope_is_403(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    // **scope 細分化**: write:reactions のみ → notes/create は 403 PERMISSION_DENIED。
    let token = issue_token_with_scopes(&pool, &["write:reactions"]).await;

    let body = json!({"i": token, "text": "x"});
    let resp = post_json(app, "/api/notes/create", body).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let v = read_json(resp).await;
    assert_eq!(v["error"]["code"], "PERMISSION_DENIED");
    assert_eq!(v["error"]["kind"], "permission");
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
            license: None,
            is_sensitive: false,
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
async fn reactions_create_without_scope_is_403(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    // scope 細分化: write:notes のみ → reactions/create は 403 PERMISSION_DENIED。
    let token = issue_token_with_scopes(&pool, &["write:notes"]).await;

    let resp = post_json(
        app,
        "/api/notes/reactions/create",
        json!({"i": token, "noteId": "1", "reaction": "👍"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let v = read_json(resp).await;
    assert_eq!(v["error"]["code"], "PERMISSION_DENIED");
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
    // follow 直後は remote 側の Accept 前なので pending ── Misskey wire の
    // `hasPendingFollowRequestFromYou` が true で載る。
    assert_eq!(v["hasPendingFollowRequestFromYou"], true, "{v}");
    assert_eq!(v["hasPendingFollowRequestToYou"], false, "{v}");
    assert_eq!(
        v["isFollowing"], false,
        "pending は isFollowing にならない: {v}"
    );

    // delivery_queue に Follow 行が積まれている。
    let queued: i64 = sqlx::query_scalar!("SELECT count(*) FROM delivery_queue")
        .fetch_one(&pool)
        .await
        .unwrap()
        .unwrap_or(0);
    assert!(queued >= 1, "delivery_queue should have at least 1 row");
}

/// **#348 系**: 相手 (bob) が既に alice (= ローカル actor) を accepted で
/// follow している状態で alice が follow を送ると、レスポンスの `UserDetailed`
/// に `isFollowed=true` (「フォローされています」) が載る ──
/// `following/create` 成功レスポンスをクライアントがそのままプロフィール描画に
/// 使っても関係が正しく出ることを lock する。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn following_create_response_reports_is_followed_when_mutual(pool: PgPool) {
    let alice = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let bob = seed_remote_actor(&pool, "misskey.io", "bob").await;
    // bob → alice が accepted (既にフォローされています)。
    accepted_follow(&pool, bob, alice).await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:following"]).await;

    let resp = post_json(
        app,
        "/api/following/create",
        json!({"i": token, "userId": bob.to_string()}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_json(resp).await;
    assert_eq!(v["username"], "bob");
    assert_eq!(v["isFollowed"], true, "bob already follows alice: {v}");
    // alice → bob は今回送ったばかりで pending。
    assert_eq!(v["hasPendingFollowRequestFromYou"], true, "{v}");
    assert_eq!(
        v["isFollowing"], false,
        "pending は isFollowing にならない: {v}"
    );
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
async fn following_create_without_scope_is_403(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let target_id = seed_remote_actor(&pool, "misskey.io", "bob").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    // scope 細分化: write:notes のみ → following/create は 403 PERMISSION_DENIED。
    let token = issue_token_with_scopes(&pool, &["write:notes"]).await;

    let resp = post_json(
        app,
        "/api/following/create",
        json!({"i": token, "userId": target_id.to_string()}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let v = read_json(resp).await;
    assert_eq!(v["error"]["code"], "PERMISSION_DENIED");
}

// ─── アナーキー (ignore_scope) ─────────────────────────────────────────

/// `ignore_scope = true` (= アナーキー, お一人様 default) では **空 permissions
/// token** でも `following/create` が 200 になる ── Aria の follow が
/// `write:following` scope 不足で 401 になる問題の根本解消 (fix/miauth-follow-failure)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn anarchy_ignores_scope_for_follow(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let target_id = seed_remote_actor(&pool, "misskey.io", "bob").await;
    let state = AppState::from_pool(
        pool.clone(),
        common::make_config_anarchy("sakurasato.test", "alice"),
    );
    let app = router_for(&state);
    // 空 permissions ── アナーキーなら関係ない。
    let token = issue_token_with_scopes(&pool, &[]).await;

    let resp = post_json(
        app,
        "/api/following/create",
        json!({"i": token, "userId": target_id.to_string()}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_json(resp).await;
    assert_eq!(v["username"], "bob");
    // Follow 配送も走る。
    let queued: i64 = sqlx::query_scalar!("SELECT count(*) FROM delivery_queue")
        .fetch_one(&pool)
        .await
        .unwrap()
        .unwrap_or(0);
    assert!(queued >= 1, "delivery_queue should have at least 1 row");
}

/// アナーキーでも **無効 token は 401** (= hash lookup は常に検査。revoke 済み /
/// 存在しない token で全権限が漏れない)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn anarchy_still_rejects_unknown_token(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = AppState::from_pool(
        pool.clone(),
        common::make_config_anarchy("sakurasato.test", "alice"),
    );
    let app = router_for(&state);

    let resp = post_json(
        app,
        "/api/following/create",
        json!({"i": "nonexistent-raw-token", "userId": "1"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let v = read_json(resp).await;
    assert_eq!(v["error"]["code"], "AUTHENTICATION_FAILED");
}

/// アナーキーでは **全 write/read scope が通る** ── `write:reactions` のみの
/// token でも `notes/create` (= write:notes) が 200。厳密 mode (OFF) の
/// `notes_create_without_scope_is_403` と対で挙動差を lock する。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn anarchy_allows_all_scopes_for_write(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = AppState::from_pool(
        pool.clone(),
        common::make_config_anarchy("sakurasato.test", "alice"),
    );
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:reactions"]).await;

    let resp = post_json(
        app,
        "/api/notes/create",
        json!({"i": token, "text": "anarchy!"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_json(resp).await;
    assert_eq!(v["createdNote"]["text"], "anarchy!");
}

// ─── notes/renote (Iceshrimp alias) ────────────────────────────────────

/// `notes/renote { renoteId }` が remote の public note を boost (= Announce) し、
/// `createdNote` を Misskey の renote 形 (text=null / renoteId / renote) で返す。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_renote_announces_remote_note(pool: PgPool) {
    let alice = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let bob = seed_remote_actor(&pool, "misskey.io", "bob").await;
    let target_id = seed_remote_note(&pool, bob, "https://misskey.io/notes/xyz").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:notes"]).await;

    let resp = post_json(
        app,
        "/api/notes/renote",
        json!({"i": token, "renoteId": target_id.to_string()}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_json(resp).await;
    let created = &v["createdNote"];
    // renote 形: text は null、renoteId / renote が対象 note を指す。
    assert!(
        created["text"].is_null(),
        "renote text must be null: {created}"
    );
    assert_eq!(created["renoteId"], target_id.to_string());
    assert_eq!(created["renote"]["id"], target_id.to_string());

    // announce 行が立つ (= boost が記録される)。
    let row = repo::announce::get_by_pair(&pool, target_id, alice)
        .await
        .unwrap();
    assert!(
        row.is_some(),
        "an announce row must be created for the renote"
    );
}

/// 自分の note を renote しようとすると `400 CANNOT_RENOTE` (= 自己 boost は
/// Mastodon / Sakurasato で禁止、`local_api` の 422 を 400 にマップ)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_renote_own_note_returns_error(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:notes"]).await;

    let v = read_json(
        post_json(
            app.clone(),
            "/api/notes/create",
            json!({"i": token, "text": "my own note"}),
        )
        .await,
    )
    .await;
    let own_id = v["createdNote"]["id"].as_str().unwrap().to_string();

    let resp = post_json(
        app,
        "/api/notes/renote",
        json!({"i": token, "renoteId": own_id}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let e = read_json(resp).await;
    assert_eq!(e["error"]["code"], "CANNOT_RENOTE");
}

/// 存在しない note を renote すると `404 NO_SUCH_NOTE`。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_renote_unknown_returns_404(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:notes"]).await;

    let resp = post_json(
        app,
        "/api/notes/renote",
        json!({"i": token, "renoteId": "999999"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let e = read_json(resp).await;
    assert_eq!(e["error"]["code"], "NO_SUCH_NOTE");
}

/// Aria クラッシュレポート由来の回帰テスト: renote (boost) は `announce` テーブル
/// 持ちで `rn:<announce_id>` という note とは別 id 名前空間を持つ。`notes/delete`
/// が素の note 同様 `i64` 直接パースしか解さないと、renote を削除しようとした
/// ときに常に `404 NO_SUCH_NOTE` になり、クライアント側で未捕捉例外化していた。
/// `rn:` prefix を Undo Announce 経路 ([`local_api::renotes::build_and_dispatch_undo`]
/// 共有) に振り分け、`204 No Content` + `announce` 行削除まで通ることを確認する。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_delete_renote_removes_announce_row_and_returns_204(pool: PgPool) {
    let alice = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let bob = seed_remote_actor(&pool, "misskey.io", "bob").await;
    let target_id = seed_remote_note(&pool, bob, "https://misskey.io/notes/xyz").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:notes"]).await;

    let v = read_json(
        post_json(
            app.clone(),
            "/api/notes/renote",
            json!({"i": token, "renoteId": target_id.to_string()}),
        )
        .await,
    )
    .await;
    let renote_id = v["createdNote"]["id"].as_str().unwrap().to_string();
    assert!(
        renote_id.starts_with("rn:"),
        "renote id must use the rn: namespace: {renote_id}"
    );

    let resp = post_json(
        app,
        "/api/notes/delete",
        json!({"i": token, "noteId": renote_id}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let row = repo::announce::get_by_pair(&pool, target_id, alice)
        .await
        .unwrap();
    assert!(row.is_none(), "announce row must be deleted");
}

/// 存在しない announce id (`rn:<id>`) を `notes/delete` しようとすると
/// `404 NO_SUCH_NOTE`。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_delete_renote_unknown_returns_404(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:notes"]).await;

    let resp = post_json(
        app,
        "/api/notes/delete",
        json!({"i": token, "noteId": "rn:999999"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let e = read_json(resp).await;
    assert_eq!(e["error"]["code"], "NO_SUCH_NOTE");
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

// ─── リスト機能 (Mastodon/Misskey 互換, `users/lists/*`) ────────────────────

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn users_lists_create_returns_miss_user_list(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:account"]).await;

    let resp = post_json(
        app,
        "/api/users/lists/create",
        json!({"i": token, "name": "friends"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_json(resp).await;
    assert_eq!(v["name"], "friends");
    assert_eq!(v["userIds"], json!([]));
    assert!(v["id"].is_string());
    assert!(v["createdAt"].is_string());
}

/// 自分自身は follow していなくても `users/lists/push` で無条件に追加できる
/// (`repo::user_list::add_member` の self 例外)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn users_lists_push_allows_self_without_follow(pool: PgPool) {
    let me = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:account", "read:account"]).await;

    let created = read_json(
        post_json(
            app.clone(),
            "/api/users/lists/create",
            json!({"i": token, "name": "with me"}),
        )
        .await,
    )
    .await;
    let list_id = created["id"].as_str().unwrap().to_string();

    let resp = post_json(
        app.clone(),
        "/api/users/lists/push",
        json!({"i": token, "listId": list_id, "userId": me.to_string()}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let show = read_json(
        post_json(
            app,
            "/api/users/lists/show",
            json!({"i": token, "listId": list_id}),
        )
        .await,
    )
    .await;
    assert_eq!(show["userIds"], json!([me.to_string()]));
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn users_lists_push_requires_accepted_follow_then_succeeds(pool: PgPool) {
    let me = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let bob = seed_remote_actor(&pool, "remote.test", "bob").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:account", "read:account"]).await;

    let created = read_json(
        post_json(
            app.clone(),
            "/api/users/lists/create",
            json!({"i": token, "name": "friends"}),
        )
        .await,
    )
    .await;
    let list_id = created["id"].as_str().unwrap().to_string();

    // bob をまだ follow していないので NOT_FOLLOWING。
    let resp = post_json(
        app.clone(),
        "/api/users/lists/push",
        json!({"i": token, "listId": list_id, "userId": bob.to_string()}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(read_json(resp).await["error"]["code"], "NOT_FOLLOWING");

    accepted_follow(&pool, me, bob).await;

    let resp = post_json(
        app.clone(),
        "/api/users/lists/push",
        json!({"i": token, "listId": list_id, "userId": bob.to_string()}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let show = read_json(
        post_json(
            app.clone(),
            "/api/users/lists/show",
            json!({"i": token, "listId": list_id}),
        )
        .await,
    )
    .await;
    assert_eq!(show["userIds"], json!([bob.to_string()]));

    // pull で削除できる。
    let resp = post_json(
        app.clone(),
        "/api/users/lists/pull",
        json!({"i": token, "listId": list_id, "userId": bob.to_string()}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let show = read_json(
        post_json(
            app,
            "/api/users/lists/show",
            json!({"i": token, "listId": list_id}),
        )
        .await,
    )
    .await;
    assert_eq!(show["userIds"], json!([]));
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn users_lists_delete_removes_list(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:account", "read:account"]).await;

    let created = read_json(
        post_json(
            app.clone(),
            "/api/users/lists/create",
            json!({"i": token, "name": "temp"}),
        )
        .await,
    )
    .await;
    let list_id = created["id"].as_str().unwrap().to_string();

    let resp = post_json(
        app.clone(),
        "/api/users/lists/delete",
        json!({"i": token, "listId": list_id}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = post_json(
        app,
        "/api/users/lists/show",
        json!({"i": token, "listId": list_id}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(read_json(resp).await["error"]["code"], "NO_SUCH_LIST");
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn users_lists_create_without_scope_is_403(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    // read:account のみ (= write:account 不足) → 403 PERMISSION_DENIED。
    let token = issue_token_with_scopes(&pool, &["read:account"]).await;

    let resp = post_json(
        app,
        "/api/users/lists/create",
        json!({"i": token, "name": "friends"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let v = read_json(resp).await;
    assert_eq!(v["error"]["code"], "PERMISSION_DENIED");
}

// ─── following/requests/{list,accept,reject,cancel} (Aria FollowRequestsNotifier fix) ──

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn following_requests_list_returns_pending(pool: PgPool) {
    let alice_id = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let bob_id = seed_remote_actor(&pool, "misskey.io", "bob").await;
    pending_follow(&pool, bob_id, alice_id).await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["read:account"]).await;

    let resp = post_json(app, "/api/following/requests/list", json!({"i": token})).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_json(resp).await;
    let items = v.as_array().expect("response must be a JSON array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["follower"]["username"], "bob");
    assert_eq!(items[0]["follower"]["host"], "misskey.io");
    assert_eq!(items[0]["followee"]["username"], "alice");
    assert!(items[0]["id"].is_string());
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn following_requests_list_without_scope_is_403(pool: PgPool) {
    let alice_id = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let bob_id = seed_remote_actor(&pool, "misskey.io", "bob").await;
    pending_follow(&pool, bob_id, alice_id).await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    // write:following のみ (= read:account 不足) → 403 PERMISSION_DENIED。
    let token = issue_token_with_scopes(&pool, &["write:following"]).await;

    let resp = post_json(app, "/api/following/requests/list", json!({"i": token})).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let v = read_json(resp).await;
    assert_eq!(v["error"]["code"], "PERMISSION_DENIED");
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn following_requests_accept_moves_state_and_enqueues_response(pool: PgPool) {
    let alice_id = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let bob_id = seed_remote_actor(&pool, "misskey.io", "bob").await;
    let follow_id = pending_follow(&pool, bob_id, alice_id).await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:following"]).await;

    let resp = post_json(
        app,
        "/api/following/requests/accept",
        json!({"i": token, "userId": bob_id.to_string()}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let row = repo::follow::get_by_id(&pool, follow_id)
        .await
        .unwrap()
        .expect("follow row must still exist");
    assert_eq!(row.state, "accepted");

    let queued: i64 = sqlx::query_scalar!("SELECT count(*) FROM delivery_queue")
        .fetch_one(&pool)
        .await
        .unwrap()
        .unwrap_or(0);
    assert!(
        queued >= 1,
        "delivery_queue should have the Accept activity"
    );
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn following_requests_reject_sets_rejected(pool: PgPool) {
    let alice_id = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let bob_id = seed_remote_actor(&pool, "misskey.io", "bob").await;
    let follow_id = pending_follow(&pool, bob_id, alice_id).await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:following"]).await;

    let resp = post_json(
        app,
        "/api/following/requests/reject",
        json!({"i": token, "userId": bob_id.to_string()}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let row = repo::follow::get_by_id(&pool, follow_id)
        .await
        .unwrap()
        .expect("follow row must still exist");
    assert_eq!(row.state, "rejected");
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn following_requests_accept_unknown_user_returns_404(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let bob_id = seed_remote_actor(&pool, "misskey.io", "bob").await;
    // pending 行を作らない ── bob からの Follow は存在しない。
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:following"]).await;

    let resp = post_json(
        app,
        "/api/following/requests/accept",
        json!({"i": token, "userId": bob_id.to_string()}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        read_json(resp).await["error"]["code"],
        "FOLLOW_REQUEST_NOT_FOUND"
    );
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn following_requests_accept_already_accepted_returns_400(pool: PgPool) {
    let alice_id = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let bob_id = seed_remote_actor(&pool, "misskey.io", "bob").await;
    // 既に accepted な行 (= 二重 accept のレース / リトライ)。
    accepted_follow(&pool, bob_id, alice_id).await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:following"]).await;

    let resp = post_json(
        app,
        "/api/following/requests/accept",
        json!({"i": token, "userId": bob_id.to_string()}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        read_json(resp).await["error"]["code"],
        "FOLLOW_REQUEST_NOT_FOUND"
    );
}

/// `following/requests/cancel` 成功 ── me (= alice) が remote に送った pending
/// Follow を取り下げると、follow 行が消え、Undo Follow が `delivery_queue` に
/// 積まれる。対象行の向きは `pending_follow(alice, bob)` (= follower=alice)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn following_requests_cancel_deletes_row_and_enqueues_undo(pool: PgPool) {
    let alice_id = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let bob_id = seed_remote_actor(&pool, "misskey.io", "bob").await;
    let follow_id = pending_follow(&pool, alice_id, bob_id).await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:following"]).await;

    let resp = post_json(
        app,
        "/api/following/requests/cancel",
        json!({"i": token, "userId": bob_id.to_string()}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let row = repo::follow::get_by_id(&pool, follow_id).await.unwrap();
    assert!(row.is_none(), "follow row must be deleted by cancel");

    // delivery_queue に Undo Follow が積まれ、object が取り下げ対象の
    // (me → bob) ペアになっている。runtime クエリなので .sqlx offline cache
    // は不要 (notes/delete テストと同じ流儀)。
    let activity: JsonValue = sqlx::query_scalar(
        "SELECT activity FROM delivery_queue WHERE activity->>'type' = 'Undo' ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .expect("an Undo activity must be enqueued");
    assert_eq!(activity["type"], "Undo");
    assert_eq!(activity["object"]["type"], "Follow");
    assert_eq!(
        activity["object"]["actor"],
        "https://sakurasato.test/users/alice"
    );
    assert_eq!(activity["object"]["object"], "https://misskey.io/users/bob");
}

/// `following/requests/cancel` は pending 限定 ── accepted 済みのフォローを
/// cancel で消してしまう事故を防ぐ (state を問わず削除する
/// `delete_follow_core` を pending でガードするため)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn following_requests_cancel_already_accepted_returns_400(pool: PgPool) {
    let alice_id = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let bob_id = seed_remote_actor(&pool, "misskey.io", "bob").await;
    // accepted な行 (= cancel 対象外)。
    accepted_follow(&pool, alice_id, bob_id).await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:following"]).await;

    let resp = post_json(
        app,
        "/api/following/requests/cancel",
        json!({"i": token, "userId": bob_id.to_string()}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        read_json(resp).await["error"]["code"],
        "FOLLOW_REQUEST_NOT_FOUND"
    );

    let row = repo::follow::get_by_pair(&pool, alice_id, bob_id)
        .await
        .unwrap();
    assert!(
        row.is_some(),
        "accepted follow row must survive a failed cancel"
    );
}

/// `following/requests/cancel` で行が無い (誰も follow していない) → 400。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn following_requests_cancel_no_row_returns_400(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let bob_id = seed_remote_actor(&pool, "misskey.io", "bob").await;
    // alice → bob の follow 行を作らない。
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:following"]).await;

    let resp = post_json(
        app,
        "/api/following/requests/cancel",
        json!({"i": token, "userId": bob_id.to_string()}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        read_json(resp).await["error"]["code"],
        "FOLLOW_REQUEST_NOT_FOUND"
    );
}

/// `following/requests/cancel` の scope 不足 (read:account のみ) → 403。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn following_requests_cancel_without_scope_is_403(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let bob_id = seed_remote_actor(&pool, "misskey.io", "bob").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["read:account"]).await;

    let resp = post_json(
        app,
        "/api/following/requests/cancel",
        json!({"i": token, "userId": bob_id.to_string()}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let v = read_json(resp).await;
    assert_eq!(v["error"]["code"], "PERMISSION_DENIED");
}

// ─── i/update (プロフィール編集 / Aria `INotifier` crash fix) ──────────────

async fn seed_media(pool: &PgPool, owner: i64, key: &str) -> i64 {
    repo::media::insert(
        pool,
        repo::media::NewMedia {
            storage_key: key.into(),
            media_type: "image/webp".into(),
            width: 256,
            height: 256,
            byte_size: 2048,
            kind: "attachment".into(),
            alt_text: None,
            owner_actor_id: owner,
            duration_ms: None,
        },
    )
    .await
    .expect("seed media")
    .id
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn i_update_sets_description_and_returns_me_detailed(pool: PgPool) {
    let alice_id = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:account"]).await;

    let resp = post_json(
        app,
        "/api/i/update",
        json!({"i": token, "description": "new bio"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_json(resp).await;
    // Aria の `INotifier` は応答を `MeDetailed.fromJson` でパースするため、
    // required bool が欠落すると crash する ── ここで代表的な数件を確認する。
    assert_eq!(v["description"], "new bio");
    assert!(v["isBot"].is_boolean());
    assert!(v["isCat"].is_boolean());
    assert!(v["isAdmin"].is_boolean());

    let row = repo::actor::get_by_id(&pool, alice_id)
        .await
        .unwrap()
        .expect("actor row must exist");
    assert_eq!(row.summary.as_deref(), Some("new bio"));
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn i_update_clears_description_with_null(pool: PgPool) {
    let alice_id = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:account"]).await;

    let resp = post_json(
        app,
        "/api/i/update",
        json!({"i": token, "description": null}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(read_json(resp).await["description"].is_null());

    let row = repo::actor::get_by_id(&pool, alice_id)
        .await
        .unwrap()
        .expect("actor row must exist");
    assert_eq!(row.summary, None);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn i_update_sets_birthday(pool: PgPool) {
    let alice_id = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:account"]).await;

    let resp = post_json(
        app,
        "/api/i/update",
        json!({"i": token, "birthday": "2000-01-02"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(read_json(resp).await["birthday"], "2000-01-02");

    let row = repo::actor::get_by_id(&pool, alice_id)
        .await
        .unwrap()
        .expect("actor row must exist");
    assert_eq!(row.birthday.as_deref(), Some("2000-01-02"));
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn i_update_rejects_malformed_birthday(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:account"]).await;

    let resp = post_json(
        app,
        "/api/i/update",
        json!({"i": token, "birthday": "not-a-date"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(read_json(resp).await["error"]["code"], "INVALID_PARAM");
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn i_update_clears_birthday_with_null(pool: PgPool) {
    let alice_id = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:account"]).await;

    let _ = post_json(
        app.clone(),
        "/api/i/update",
        json!({"i": token.clone(), "birthday": "2000-01-02"}),
    )
    .await;
    let resp = post_json(app, "/api/i/update", json!({"i": token, "birthday": null})).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(read_json(resp).await["birthday"].is_null());

    let row = repo::actor::get_by_id(&pool, alice_id)
        .await
        .unwrap()
        .expect("actor row must exist");
    assert_eq!(row.birthday, None);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn i_update_sets_location_lang_and_followed_message(pool: PgPool) {
    let alice_id = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:account"]).await;

    let resp = post_json(
        app,
        "/api/i/update",
        json!({
            "i": token,
            "location": "Kyoto",
            "lang": "ja-JP",
            "followedMessage": "よろしくお願いします",
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_json(resp).await;
    assert_eq!(v["location"], "Kyoto");
    assert_eq!(v["lang"], "ja-JP");
    assert_eq!(v["followedMessage"], "よろしくお願いします");

    let row = repo::actor::get_by_id(&pool, alice_id)
        .await
        .unwrap()
        .expect("actor row must exist");
    assert_eq!(row.location.as_deref(), Some("Kyoto"));
    assert_eq!(row.lang.as_deref(), Some("ja-JP"));
    assert_eq!(
        row.followed_message.as_deref(),
        Some("よろしくお願いします")
    );
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn i_update_sets_fields(pool: PgPool) {
    let alice_id = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:account"]).await;

    let resp = post_json(
        app,
        "/api/i/update",
        json!({
            "i": token,
            "fields": [
                {"name": "Website", "value": "https://example.test"},
                {"name": "Pronouns", "value": "she/her"},
            ],
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_json(resp).await;
    assert_eq!(v["fields"][0]["name"], "Website");
    assert_eq!(v["fields"][0]["value"], "https://example.test");
    assert_eq!(v["fields"][1]["name"], "Pronouns");

    let row = repo::actor::get_by_id(&pool, alice_id)
        .await
        .unwrap()
        .expect("actor row must exist");
    assert_eq!(row.fields.0.len(), 2);
    assert_eq!(row.fields.0[0].name, "Website");
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn i_update_fields_rejects_too_many_entries(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:account"]).await;

    let fields: Vec<_> = (0..5)
        .map(|i| json!({"name": format!("f{i}"), "value": "v"}))
        .collect();
    let resp = post_json(app, "/api/i/update", json!({"i": token, "fields": fields})).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(read_json(resp).await["error"]["code"], "INVALID_PARAM");
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn i_update_fields_empty_array_clears(pool: PgPool) {
    let alice_id = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:account"]).await;

    let _ = post_json(
        app.clone(),
        "/api/i/update",
        json!({"i": token.clone(), "fields": [{"name": "a", "value": "b"}]}),
    )
    .await;
    let resp = post_json(app, "/api/i/update", json!({"i": token, "fields": []})).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(read_json(resp).await["fields"].as_array().unwrap().len(), 0);

    let row = repo::actor::get_by_id(&pool, alice_id)
        .await
        .unwrap()
        .expect("actor row must exist");
    assert!(row.fields.0.is_empty());
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn i_update_unknown_fields_are_accepted_and_ignored(pool: PgPool) {
    // Misskey クライアントの設定画面は40以上の項目を一括 PATCH で送る。
    // Sakurasato がバックエンド列を持たない項目 (isBot 等) で 400 を返すと
    // 画面全体が壊れるため、黙って無視して 200 を返すことを確認する。
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:account"]).await;

    let resp = post_json(
        app,
        "/api/i/update",
        json!({
            "i": token,
            "isBot": true,
            "isExplorable": false,
            "mutedWords": [],
            "fields": [{"name": "a", "value": "b"}],
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn i_update_name_exceeds_limit_returns_400(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:account"]).await;

    let resp = post_json(
        app,
        "/api/i/update",
        json!({"i": token, "name": "x".repeat(101)}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(read_json(resp).await["error"]["code"], "INVALID_PARAM");
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn i_update_without_write_account_scope_is_403(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    // read:account だけでは書き込みできない → 403 PERMISSION_DENIED。
    let token = issue_token_with_scopes(&pool, &["read:account"]).await;

    let resp = post_json(
        app,
        "/api/i/update",
        json!({"i": token, "description": "x"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let v = read_json(resp).await;
    assert_eq!(v["error"]["code"], "PERMISSION_DENIED");
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn i_update_avatar_id_sets_icon_url(pool: PgPool) {
    let alice_id = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let media_id = seed_media(&pool, alice_id, "avatar.webp").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:account"]).await;

    let resp = post_json(
        app,
        "/api/i/update",
        json!({"i": token, "avatarId": media_id.to_string()}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_json(resp).await;
    assert_eq!(v["avatarUrl"], "https://sakurasato.test/media/avatar.webp");

    let row = repo::actor::get_by_id(&pool, alice_id)
        .await
        .unwrap()
        .expect("actor row must exist");
    assert_eq!(
        row.icon_url.as_deref(),
        Some("https://sakurasato.test/media/avatar.webp")
    );
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn i_update_avatar_id_not_owned_returns_404(pool: PgPool) {
    let _alice_id = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let bob_id = seed_remote_actor(&pool, "misskey.io", "bob").await;
    let bob_media_id = seed_media(&pool, bob_id, "bob-avatar.webp").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:account"]).await;

    let resp = post_json(
        app,
        "/api/i/update",
        json!({"i": token, "avatarId": bob_media_id.to_string()}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(read_json(resp).await["error"]["code"], "NO_SUCH_FILE");
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn i_update_is_locked_flips_flag_and_enqueues_update_for_followers(pool: PgPool) {
    let alice_id = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let bob_id = seed_remote_actor(&pool, "misskey.io", "bob").await;
    accepted_follow(&pool, bob_id, alice_id).await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:account"]).await;

    let resp = post_json(app, "/api/i/update", json!({"i": token, "isLocked": true})).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(read_json(resp).await["isLocked"], true);

    let row = repo::actor::get_by_id(&pool, alice_id)
        .await
        .unwrap()
        .expect("actor row must exist");
    assert!(row.manually_approves_followers);

    // 鍵アカ切替 (= `actor lock`) と同じく、bob の inbox に Update が積まれる。
    let queued: i64 = sqlx::query_scalar!("SELECT count(*) FROM delivery_queue")
        .fetch_one(&pool)
        .await
        .unwrap()
        .unwrap_or(0);
    assert!(
        queued >= 1,
        "delivery_queue should have the Update activity"
    );
}
