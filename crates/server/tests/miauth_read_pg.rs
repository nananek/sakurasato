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
    seed_note_with_audience(pool, actor_id, host, content, visibility, vec![], vec![]).await
}

/// `seed_note` の汎用版。`to_recipients` / `cc_recipients` を指定できる。
/// **#165 round-2 review #3** の `direct` visibility テストで「viewer の
/// `ap_id` が audience に含まれているか」を切り分けるために使う。
async fn seed_note_with_audience(
    pool: &PgPool,
    actor_id: i64,
    host: &str,
    content: &str,
    visibility: Visibility,
    to_recipients: Vec<String>,
    cc_recipients: Vec<String>,
) -> i64 {
    let ap_id = format!("https://{host}/notes/pending-{}", Uuid::new_v4());
    // visibility が direct のときは default `Public` 宛 を上書きする。
    let to = if !to_recipients.is_empty() {
        to_recipients
    } else if matches!(visibility, Visibility::Direct) {
        Vec::new()
    } else {
        vec!["https://www.w3.org/ns/activitystreams#Public".into()]
    };
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
        to_recipients: to,
        cc_recipients,
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

/// remote actor を 1 件作る (= `direct`/`followers` visibility のアクセス制御
/// テストで「自分以外の author」を用意するため)。`is_local: false`、秘密鍵なし。
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
        // remote actor は秘密鍵を持たない。
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

/// follower → followed の `follow` 行を `accepted` 状態で 1 件作る。
/// `notes/show` の `followers` visibility テストで使用。
#[allow(clippy::similar_names, reason = "follower / followed は AP 用語")]
async fn seed_accepted_follow(pool: &PgPool, follower: i64, followed: i64) {
    let ap_id = format!("https://test/follow/{follower}-{followed}");
    let row = repo::follow::insert_pending(pool, &ap_id, follower, followed)
        .await
        .expect("seed pending follow");
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
    // **#165 round-2 fix**: local actor のノートは `user.host: null`
    // (Misskey wire 仕様 ── Milktea 等が `host != null` の user を
    // remote 扱いするのを防ぐ)。
    assert!(
        notes[0]["user"]["host"].is_null(),
        "local actor note must have user.host: null, got {:?}",
        notes[0]["user"]["host"],
    );
    // visibility: "public" → "public"
    assert_eq!(notes[0]["visibility"], "public");
    // mentions / fileIds / files / reactions / emojis are always present.
    assert!(notes[0]["mentions"].is_array());
    assert!(notes[0]["fileIds"].is_array());
    assert!(notes[0]["files"].is_array());
    assert!(notes[0]["reactions"].is_object());
    assert!(notes[0]["emojis"].is_object());
}

/// followee の renote (= `Announce`) が home timeline に **renote `MissNote`** と
/// して出る (#: 報告バグ「リノートがリノートとして流れてこない」の修正)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn timeline_includes_followee_renote_as_renote(pool: PgPool) {
    let alice = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let bob = seed_remote_actor(&pool, "remote.test", "bob").await;
    // alice が bob を follow している (= bob の Announce を home に取り込む)。
    seed_accepted_follow(&pool, alice, bob).await;

    // ある note を bob が boost する。boost 時刻を note より後にして先頭に来させる。
    let note_id = seed_note(
        &pool,
        alice,
        "sakurasato.test",
        "original post",
        Visibility::Public,
    )
    .await;
    let announce = repo::announce::insert_or_get(
        &pool,
        "https://remote.test/users/bob/activities/announce-tl-1",
        note_id,
        bob,
        chrono::Utc::now() + chrono::Duration::seconds(10),
    )
    .await
    .unwrap();

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

    // renote (boost 時刻が新しい) が先頭。
    let rn = &notes[0];
    assert_eq!(
        rn["id"],
        format!("rn:{}", announce.id),
        "renote MissNote id must be namespaced rn:<announce_id>: {rn}"
    );
    assert!(
        rn["text"].is_null(),
        "renote MissNote text must be null: {rn}"
    );
    assert_eq!(rn["user"]["username"], "bob", "renoter must be bob: {rn}");
    // nest した元 note は素の note id + 本文。
    assert_eq!(rn["renoteId"], note_id.to_string());
    assert_eq!(rn["renote"]["id"], note_id.to_string());
    assert_eq!(rn["renote"]["text"], "original post");

    // 2 件目は元 note 自体 (alice の投稿)。
    assert_eq!(notes[1]["id"], note_id.to_string());
    assert_eq!(notes[1]["text"], "original post");
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
    // #197: 401 も nested body `{"error":{"code":"AUTHENTICATION_FAILED",...}}` で
    // 返す (= forbidden / 404 / 500 と shape を揃え、client が code を拾える)。
    let err = read_json(resp).await;
    assert_eq!(err["error"]["code"], "AUTHENTICATION_FAILED");
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
            license: None,
            is_sensitive: false,
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
            license: None,
            is_sensitive: false,
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
    // #197: users/show の 404 は Misskey wire の固有 code `NO_SUCH_USER` を
    // nested body で返す (= helper 集約後もこの code を保持することを lock)。
    let err = read_json(resp).await;
    assert_eq!(err["error"]["code"], "NO_SUCH_USER");
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

// ─── #165 round-2 review #3: followers / direct visibility access control ─

/// `followers` visibility: viewer (= local actor) が author を **未 follow** の
/// とき `notes/show` は **404** を返す。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_show_followers_unfollowed_viewer_returns_404(pool: PgPool) {
    let _viewer_id = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let author_id = seed_remote_actor(&pool, "misskey.io", "bob").await;
    let note_id = seed_note_with_audience(
        &pool,
        author_id,
        "misskey.io",
        "followers-only",
        Visibility::Followers,
        vec!["https://misskey.io/users/bob/followers".into()],
        vec![],
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
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// `followers` visibility: viewer が author を **accepted で follow** している
/// とき `notes/show` は **200** + `MissNote` を返す。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_show_followers_followed_viewer_returns_200(pool: PgPool) {
    let viewer_id = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let author_id = seed_remote_actor(&pool, "misskey.io", "bob").await;
    seed_accepted_follow(&pool, viewer_id, author_id).await;
    let note_id = seed_note_with_audience(
        &pool,
        author_id,
        "misskey.io",
        "followers-only",
        Visibility::Followers,
        vec!["https://misskey.io/users/bob/followers".into()],
        vec![],
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
    assert_eq!(note["text"], "followers-only");
    // followers → Misskey の `followers` (= 文字列そのまま)。
    assert_eq!(note["visibility"], "followers");
}

/// `direct` visibility: viewer が audience に **居ない** とき `notes/show` は
/// **404** を返す (= 自分宛 DM ではないので見えない)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_show_direct_non_audience_viewer_returns_404(pool: PgPool) {
    let _viewer_id = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let author_id = seed_remote_actor(&pool, "misskey.io", "bob").await;
    // 宛先が carol (= 自分以外の架空 user)。
    let note_id = seed_note_with_audience(
        &pool,
        author_id,
        "misskey.io",
        "dm",
        Visibility::Direct,
        vec!["https://misskey.io/users/carol".into()],
        vec![],
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
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// `direct` visibility: viewer が `to` に居る ── 200 + `MissNote` を返す。
/// Misskey 仕様で `direct` → `visibility: "specified"` (= `conv::map_visibility`)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_show_direct_audience_viewer_returns_200_as_specified(pool: PgPool) {
    let _viewer_id = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let author_id = seed_remote_actor(&pool, "misskey.io", "bob").await;
    let viewer_uri = "https://sakurasato.test/users/alice".to_string();
    let note_id = seed_note_with_audience(
        &pool,
        author_id,
        "misskey.io",
        "dm",
        Visibility::Direct,
        vec![viewer_uri],
        vec![],
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
    assert_eq!(note["text"], "dm");
    // direct → "specified" (Misskey wire 仕様)。
    assert_eq!(note["visibility"], "specified");
}

/// `direct` visibility: viewer が `cc` (= to ではなく) に居ても 200。
/// audience 判定は to/cc 両方を見る (= AS2 仕様)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_show_direct_in_cc_also_returns_200(pool: PgPool) {
    let _viewer_id = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let author_id = seed_remote_actor(&pool, "misskey.io", "bob").await;
    let viewer_uri = "https://sakurasato.test/users/alice".to_string();
    let note_id = seed_note_with_audience(
        &pool,
        author_id,
        "misskey.io",
        "dm-via-cc",
        Visibility::Direct,
        vec!["https://misskey.io/users/carol".into()],
        vec![viewer_uri],
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
    assert_eq!(note["text"], "dm-via-cc");
}

// ─── M14 #170: HTML タグが MissNote.text に流れない ───────────────────────

/// AP `Note.content` (= HTML) が `MissNote.text` で plain text に倒される。
/// `<p>` / `<br>` / `<a>` の最小組み合わせを 1 件投入し、Misskey クライアント
/// が UI に流せる plain text になっていることを確認 (= [`miauth::text`])。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn timeline_strips_html_tags_from_text(pool: PgPool) {
    let actor_id = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let _ = seed_note(
        &pool,
        actor_id,
        "sakurasato.test",
        // AP で実際に流れる形 ── Mastodon / Misskey が `<p>` + `<br>` で送ってくる。
        r#"<p>hello<br><a href="https://example.com">world</a></p>"#,
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
    assert_eq!(notes.len(), 1);
    // `<a href="X">world</a>` → `world` (= URL は失う、Misskey は bare URL を
    // auto-detect する設計)。`<p>` / `<br>` → 改行に倒れる。
    let text = notes[0]["text"].as_str().expect("text must be string");
    assert!(
        !text.contains('<'),
        "text must not contain HTML tags: {text:?}"
    );
    assert!(
        !text.contains('>'),
        "text must not contain HTML tags: {text:?}"
    );
    assert!(
        text.contains("hello"),
        "text must contain 'hello': {text:?}"
    );
    assert!(
        text.contains("world"),
        "text must contain 'world': {text:?}"
    );
}

/// HTML entities (= `&amp;` / `&lt;` / `&#39;` 等) が decode される。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn timeline_decodes_html_entities_in_text(pool: PgPool) {
    let actor_id = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let _ = seed_note(
        &pool,
        actor_id,
        "sakurasato.test",
        "Tom &amp; Jerry &#39;hi&#39;",
        Visibility::Public,
    )
    .await;

    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["read:account"]).await;

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
    let arr = read_json(resp).await;
    assert_eq!(arr[0]["text"], "Tom & Jerry 'hi'");
}

// ─── M14 #172: MissFile wire shape (= misskey-dart DriveFile 互換) ─────────

/// timeline で attachment 付き note を返したとき、`MissNote.files[].name` が
/// **non-null string** で、`properties` object が **必須 field として存在**
/// すること。Aria など misskey-dart 利用 client が parse できる shape。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn timeline_attachment_wire_shape_matches_misskey_dart(pool: PgPool) {
    let actor_id = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    // attachment 付き note を 1 件投入。AP `Document` に `name` (= alt text) +
    // `width`/`height` を持たせる ── これらが `MissFile.comment` /
    // `MissFile.properties.{width,height}` に伝搬する想定。
    let ap_id = format!("https://sakurasato.test/notes/pending-{}", Uuid::new_v4());
    let new = NewNote {
        ap_id: ap_id.clone(),
        actor_id,
        content: "with attachment".into(),
        language: Some("ja".into()),
        in_reply_to_ap_id: None,
        in_reply_to_note_id: None,
        summary: None,
        visibility: Visibility::Public,
        sensitive: false,
        to_recipients: vec!["https://www.w3.org/ns/activitystreams#Public".into()],
        cc_recipients: vec![],
        attachments: json!([
            {
                "type": "Document",
                "mediaType": "image/webp",
                "url": "https://sakurasato.test/media/photo-abc.webp",
                "name": "Sakura petals in spring",
                "width": 1024,
                "height": 768
            }
        ]),
        tags: json!([]),
        is_local: true,
        url: None,
        published_at: chrono::Utc::now(),
    };
    let row = repo::note::insert(&pool, new).await.expect("seed note");
    let canonical = format!("https://sakurasato.test/notes/{}", row.id);
    repo::note::set_ap_id_and_url(&pool, row.id, &canonical, &canonical)
        .await
        .unwrap();

    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["read:account"]).await;

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
    assert_eq!(resp.status(), StatusCode::OK);
    let arr = read_json(resp).await;
    let files = &arr[0]["files"];
    assert!(
        files.is_array() && !files.as_array().unwrap().is_empty(),
        "files array must contain at least 1 attachment"
    );
    let f = &files[0];

    // **non-null name** ── misskey-dart の `DriveFile.name: String` 要件。
    assert!(
        f["name"].is_string(),
        "files[0].name must be a JSON string, not null; got {:?}",
        f["name"]
    );
    assert_eq!(
        f["name"], "photo-abc.webp",
        "name should be URL basename (file 名)"
    );

    // AP `name` (= alt text) は `comment` に。
    assert_eq!(f["comment"], "Sakura petals in spring");

    // **properties は必須 object** ── misskey-dart の
    // `DriveFile.properties: DriveFileProperties` 要件。
    assert!(
        f["properties"].is_object(),
        "files[0].properties must be a JSON object; got {:?}",
        f["properties"]
    );
    assert_eq!(f["properties"]["width"], 1024);
    assert_eq!(f["properties"]["height"], 768);
    // 我々が emit しない field は null だが key 自体は存在する。
    assert!(f["properties"]["orientation"].is_null());
    assert!(f["properties"]["avgColor"].is_null());
}

// ─── i/notifications (#206 PR2) ──────────────────────────────────────────

async fn post(app: axum::Router, path: &str, body: serde_json::Value) -> axum::response::Response {
    app.oneshot(
        Request::post(path)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap(),
    )
    .await
    .unwrap()
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notifications_list_returns_misskey_shape(pool: PgPool) {
    use sakurasato_core::repo::notification::{self, NewNotification};

    let alice = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let bob = seed_remote_actor(&pool, "misskey.io", "bob").await;
    let note_id = seed_note(&pool, alice, "sakurasato.test", "hi", Visibility::Public).await;
    let base = chrono::Utc::now();
    // follow (note なし)。
    notification::insert(
        &pool,
        NewNotification {
            recipient_actor_id: alice,
            event_type: "follow".into(),
            notifier_actor_id: Some(bob),
            note_id: None,
            reaction: None,
            created_at: base,
        },
    )
    .await
    .unwrap();
    // reaction (note + reaction、より新しい)。
    notification::insert(
        &pool,
        NewNotification {
            recipient_actor_id: alice,
            event_type: "reaction".into(),
            notifier_actor_id: Some(bob),
            note_id: Some(note_id),
            reaction: Some("👍".into()),
            created_at: base + chrono::Duration::seconds(1),
        },
    )
    .await
    .unwrap();

    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["read:account"]).await;

    let resp = post(
        app,
        "/api/i/notifications",
        json!({"i": token, "limit": 10}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_json(resp).await;
    let arr = v.as_array().expect("notifications is a top-level array");
    assert_eq!(arr.len(), 2);

    // id DESC ── 最新は reaction。MissUser / MissNote / reaction が乗る。
    let first = &arr[0];
    assert_eq!(first["type"], "reaction");
    assert_eq!(first["reaction"], "👍");
    assert_eq!(first["user"]["username"], "bob");
    assert_eq!(first["note"]["id"], note_id.to_string());
    assert_eq!(first["isRead"], false);

    // follow 通知は user あり / note 無し。
    let follow = arr.iter().find(|n| n["type"] == "follow").unwrap();
    assert_eq!(follow["user"]["username"], "bob");
    assert!(
        follow.get("note").is_none() || follow["note"].is_null(),
        "follow notification must not carry a note"
    );
}

/// Issue #244: remote custom emoji の reaction 通知で、埋め込み note が
/// `reactionEmojis` に自鯖キャッシュ URL を載せること。Aria の通知一覧は
/// `notification.note.reactionEmojis[reaction の `@host` 付きキー]` から
/// reaction icon を解決するので、これが無いとローカル非保有の remote 絵文字が
/// 通知でも描画できない。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notifications_reaction_embeds_remote_emoji_url(pool: PgPool) {
    use sakurasato_core::repo::notification::{self, NewNotification};

    let alice = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let bob = seed_remote_actor(&pool, "remote.test", "bob").await;
    let note_id = seed_note(&pool, alice, "sakurasato.test", "hi", Visibility::Public).await;

    // remote 絵文字 `:foo@remote.test:` を学習済み (= #243 後の wire 形)。
    let emoji = repo::emoji::upsert_remote(
        &pool,
        repo::emoji::NewRemoteEmoji {
            shortcode: "foo".into(),
            ap_id: "https://remote.test/emojis/foo".into(),
            host: "remote.test".into(),
            image_key: Some("emoji/remote/remote.test/foo.webp".into()),
            media_type: "image/webp".into(),
            last_failed_at: None,
        },
    )
    .await
    .expect("seed remote emoji");
    // note への実 reaction 行 (= 集計対象)。content は #243 の `:foo@host:` 形。
    repo::reaction::insert(
        &pool,
        "https://remote.test/users/bob/r/foo",
        note_id,
        bob,
        ":foo@remote.test:",
        Some(emoji.id),
    )
    .await
    .expect("seed reaction");
    // 通知行。reaction 文字列も `:foo@remote.test:`。
    notification::insert(
        &pool,
        NewNotification {
            recipient_actor_id: alice,
            event_type: "reaction".into(),
            notifier_actor_id: Some(bob),
            note_id: Some(note_id),
            reaction: Some(":foo@remote.test:".into()),
            created_at: chrono::Utc::now(),
        },
    )
    .await
    .unwrap();

    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["read:account"]).await;

    let resp = post(
        app,
        "/api/i/notifications",
        json!({"i": token, "limit": 10}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_json(resp).await;
    let first = &v.as_array().expect("array")[0];

    assert_eq!(first["type"], "reaction");
    assert_eq!(first["reaction"], ":foo@remote.test:");
    // 埋め込み note の reactionEmojis に `@host` 付きキー → 自鯖キャッシュ URL。
    let url = &first["note"]["reactionEmojis"]["foo@remote.test"];
    assert_eq!(
        url, "https://sakurasato.test/media/emoji/remote/remote.test/foo.webp",
        "通知の埋め込み note が remote 絵文字 URL を載せること; got note={:?}",
        first["note"]
    );
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notifications_mark_all_as_read_clears_unread(pool: PgPool) {
    use sakurasato_core::repo::notification::{self, NewNotification};

    let alice = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let bob = seed_remote_actor(&pool, "misskey.io", "bob").await;
    notification::insert(
        &pool,
        NewNotification {
            recipient_actor_id: alice,
            event_type: "follow".into(),
            notifier_actor_id: Some(bob),
            note_id: None,
            reaction: None,
            created_at: chrono::Utc::now(),
        },
    )
    .await
    .unwrap();
    assert_eq!(notification::count_unread(&pool, alice).await.unwrap(), 1);

    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["read:account"]).await;

    let resp = post(
        app,
        "/api/notifications/mark-all-as-read",
        json!({"i": token}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(notification::count_unread(&pool, alice).await.unwrap(), 0);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notifications_without_token_is_401(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);

    let resp = post(app, "/api/i/notifications", json!({})).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn i_reports_unread_notifications_count(pool: PgPool) {
    use sakurasato_core::repo::notification::{self, NewNotification};

    let alice = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let bob = seed_remote_actor(&pool, "misskey.io", "bob").await;
    for _ in 0..2 {
        notification::insert(
            &pool,
            NewNotification {
                recipient_actor_id: alice,
                event_type: "follow".into(),
                notifier_actor_id: Some(bob),
                note_id: None,
                reaction: None,
                created_at: chrono::Utc::now(),
            },
        )
        .await
        .unwrap();
    }

    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["read:account"]).await;

    let resp = post(app, "/api/i", json!({"i": token})).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let me = read_json(resp).await;
    assert_eq!(me["unreadNotificationsCount"], 2);
    assert_eq!(me["hasUnreadNotification"], true);
}

// ─── drive/files (= Aria の添付アップロード / ドライブ閲覧) ──────────────────

async fn seed_media(pool: &PgPool, owner: i64, key: &str, alt: Option<&str>) -> i64 {
    repo::media::insert(
        pool,
        repo::media::NewMedia {
            storage_key: key.into(),
            media_type: "image/webp".into(),
            width: 320,
            height: 240,
            byte_size: 4096,
            kind: "attachment".into(),
            alt_text: alt.map(str::to_string),
            owner_actor_id: owner,
        },
    )
    .await
    .expect("seed media")
    .id
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn drive_files_list_and_show_return_drive_file(pool: PgPool) {
    let alice = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let _f1 = seed_media(&pool, alice, "aaaa.webp", None).await;
    let f2 = seed_media(&pool, alice, "bbbb.webp", Some("a cat")).await;

    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["read:drive"]).await;

    // list: id 降順なので f2 が先頭。DriveFile schema を検証。
    let body = json!({"i": token, "limit": 10});
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/drive/files")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let arr = read_json(resp).await;
    let files = arr.as_array().expect("drive/files returns array");
    assert_eq!(files.len(), 2);
    // id は media 行 id (= notes/create の fileIds が parse する数値)。
    assert_eq!(files[0]["id"], f2.to_string());
    assert_eq!(files[0]["type"], "image/webp");
    assert_eq!(files[0]["comment"], "a cat");
    assert_eq!(files[0]["properties"]["width"], 320);
    assert!(
        files[0]["url"]
            .as_str()
            .unwrap()
            .ends_with("/media/bbbb.webp")
    );
    assert!(files[0]["name"].is_string(), "DriveFile.name は non-null");

    // show: 自分の file。
    let body = json!({"i": token, "fileId": f2.to_string()});
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/drive/files/show")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let f = read_json(resp).await;
    assert_eq!(f["id"], f2.to_string());

    // show: 存在しない file は 404 NO_SUCH_FILE。
    let body = json!({"i": token, "fileId": "999999"});
    let resp = app
        .oneshot(
            Request::post("/api/drive/files/show")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let err = read_json(resp).await;
    assert_eq!(err["error"]["code"], "NO_SUCH_FILE");
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn drive_files_requires_read_scope(pool: PgPool) {
    let _alice = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    // write:notes だけのトークンでは read:drive が無く 401。
    let token = issue_token_with_scopes(&pool, &["write:notes"]).await;
    let body = json!({"i": token});
    let resp = app
        .oneshot(
            Request::post("/api/drive/files")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// `drive/files/update` で `comment` (= alt text) を設定 → クリアできること。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn drive_files_update_sets_and_clears_comment(pool: PgPool) {
    let alice = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let f = seed_media(&pool, alice, "upd.webp", None).await;

    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:drive"]).await;

    // 設定: comment を付ける → 200 + 反映。
    let body = json!({"i": token, "fileId": f.to_string(), "comment": "a sleepy cat"});
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/drive/files/update")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let file = read_json(resp).await;
    assert_eq!(file["id"], f.to_string());
    assert_eq!(file["comment"], "a sleepy cat");

    // 永続化を確認: 別 query (read:drive show) で読み直しても comment が残る。
    let read_token = issue_token_with_scopes(&pool, &["read:drive"]).await;
    let body = json!({"i": read_token, "fileId": f.to_string()});
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/drive/files/show")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(read_json(resp).await["comment"], "a sleepy cat");

    // クリア: comment を null → comment が消える (= MissFile では None なので
    // フィールド欠落 or null)。
    let body = json!({"i": token, "fileId": f.to_string(), "comment": null});
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/drive/files/update")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let file = read_json(resp).await;
    assert!(
        file["comment"].is_null(),
        "comment cleared → null, got {:?}",
        file["comment"]
    );

    // 他人の file は更新できない (= NO_SUCH_FILE 404)。
    let body = json!({"i": token, "fileId": "999999", "comment": "x"});
    let resp = app
        .oneshot(
            Request::post("/api/drive/files/update")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// `drive/files/update` は `write:drive` scope を要求する (read:drive では 401)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn drive_files_update_requires_write_scope(pool: PgPool) {
    let alice = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let f = seed_media(&pool, alice, "wo.webp", None).await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["read:drive"]).await;
    let body = json!({"i": token, "fileId": f.to_string(), "comment": "x"});
    let resp = app
        .oneshot(
            Request::post("/api/drive/files/update")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// `drive/files/delete` ── 未添付 file は 204 で消え、再 show は 404。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn drive_files_delete_removes_unattached(pool: PgPool) {
    let alice = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let f = seed_media(&pool, alice, "del.webp", None).await;

    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:drive"]).await;

    let body = json!({"i": token, "fileId": f.to_string()});
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/drive/files/delete")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // 消えたので show は 404。
    let read_token = issue_token_with_scopes(&pool, &["read:drive"]).await;
    let body = json!({"i": read_token, "fileId": f.to_string()});
    let resp = app
        .oneshot(
            Request::post("/api/drive/files/show")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// `drive/files/delete` ── 添付済み file は 400 `FILE_ATTACHED` で拒否し、行は残る
/// (= note のスナップショット参照を壊さない)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn drive_files_delete_rejects_attached(pool: PgPool) {
    let alice = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let f = seed_media(&pool, alice, "att.webp", None).await;
    let note = seed_note(
        &pool,
        alice,
        "sakurasato.test",
        "with image",
        Visibility::Public,
    )
    .await;
    repo::media::attach_to_note(&pool, &[f], alice, note)
        .await
        .expect("attach media");

    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:drive"]).await;

    let body = json!({"i": token, "fileId": f.to_string()});
    let resp = app
        .oneshot(
            Request::post("/api/drive/files/delete")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let err = read_json(resp).await;
    assert_eq!(err["error"]["code"], "FILE_ATTACHED");

    // 行は残っている (= まだ owner ガード経由で引ける)。
    assert!(
        repo::media::get_by_id_for_owner(&pool, f, alice)
            .await
            .unwrap()
            .is_some(),
        "attached file must survive a rejected delete"
    );
}

/// `POST /api/drive` ── capacity 0 (無制限) + 自分の media の `byte_size` 合計。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn drive_usage_returns_capacity_and_summed_usage(pool: PgPool) {
    let alice = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    // seed_media は byte_size=4096 固定。2 件 → usage=8192。
    let _ = seed_media(&pool, alice, "u1.webp", None).await;
    let _ = seed_media(&pool, alice, "u2.webp", None).await;

    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["read:drive"]).await;

    let body = json!({"i": token});
    let resp = app
        .oneshot(
            Request::post("/api/drive")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_json(resp).await;
    assert_eq!(v["capacity"], 0, "capacity 0 = unlimited (meta と整合)");
    assert_eq!(v["usage"], 8192, "2 media * 4096 bytes");
}

/// `POST /api/drive/folders` ── フォルダ概念が無いので常に空配列。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn drive_folders_returns_empty_array(pool: PgPool) {
    let _alice = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["read:drive"]).await;

    let body = json!({"i": token});
    let resp = app
        .oneshot(
            Request::post("/api/drive/folders")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_json(resp).await;
    assert_eq!(v.as_array().expect("folders is array").len(), 0);
}

/// `POST /api/drive` は `read:drive` を要求する (scope 無しトークンは 401)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn drive_usage_requires_read_scope(pool: PgPool) {
    let _alice = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["write:notes"]).await;
    let body = json!({"i": token});
    let resp = app
        .oneshot(
            Request::post("/api/drive")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ─── notes/reactions (#244 follow-up: reactor 一覧) ──────────────────────

/// `notes/reactions` が note 1 件の個別 reaction を `{id, createdAt, user, type}`
/// の配列で `id DESC` に返すこと。Unicode と remote custom emoji の両方を含める。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_reactions_returns_reactor_list(pool: PgPool) {
    let alice = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let bob = seed_remote_actor(&pool, "remote.test", "bob").await;
    let carol = seed_remote_actor(&pool, "remote.test", "carol").await;
    let note_id = seed_note(&pool, alice, "sakurasato.test", "hi", Visibility::Public).await;

    // bob: Unicode 👍 (先に挿入 → id 小)。
    repo::reaction::insert(&pool, "https://remote.test/r/1", note_id, bob, "👍", None)
        .await
        .expect("seed unicode reaction");
    // carol: remote custom emoji :foo@remote.test: (後 → id 大 → DESC で先頭)。
    let emoji = repo::emoji::upsert_remote(
        &pool,
        repo::emoji::NewRemoteEmoji {
            shortcode: "foo".into(),
            ap_id: "https://remote.test/emojis/foo".into(),
            host: "remote.test".into(),
            image_key: Some("emoji/remote/remote.test/foo.webp".into()),
            media_type: "image/webp".into(),
            last_failed_at: None,
        },
    )
    .await
    .expect("seed remote emoji");
    repo::reaction::insert(
        &pool,
        "https://remote.test/r/2",
        note_id,
        carol,
        ":foo@remote.test:",
        Some(emoji.id),
    )
    .await
    .expect("seed custom reaction");

    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["read:account"]).await;

    let resp = post(
        app,
        "/api/notes/reactions",
        json!({"i": token, "noteId": note_id.to_string(), "limit": 20}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_json(resp).await;
    let arr = v.as_array().expect("top-level array");
    assert_eq!(arr.len(), 2, "two reactors; got {arr:?}");

    // id DESC ── carol (custom emoji) が先頭。
    assert_eq!(arr[0]["type"], ":foo@remote.test:");
    assert_eq!(arr[0]["user"]["username"], "carol");
    assert!(arr[0]["id"].is_string(), "id は string");
    assert!(
        arr[0]["createdAt"].as_str().is_some(),
        "createdAt は ISO8601 string"
    );
    // user (UserLite) は non-null かつ必須フィールドを持つ。
    assert!(arr[0]["user"]["id"].is_string());

    assert_eq!(arr[1]["type"], "👍");
    assert_eq!(arr[1]["user"]["username"], "bob");
}

/// `type` フィルタが content 完全一致で効くこと。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_reactions_type_filter(pool: PgPool) {
    let alice = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let bob = seed_remote_actor(&pool, "remote.test", "bob").await;
    let carol = seed_remote_actor(&pool, "remote.test", "carol").await;
    let note_id = seed_note(&pool, alice, "sakurasato.test", "hi", Visibility::Public).await;
    repo::reaction::insert(&pool, "https://remote.test/r/1", note_id, bob, "👍", None)
        .await
        .unwrap();
    repo::reaction::insert(&pool, "https://remote.test/r/2", note_id, carol, "❤", None)
        .await
        .unwrap();

    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["read:account"]).await;

    let resp = post(
        app,
        "/api/notes/reactions",
        json!({"i": token, "noteId": note_id.to_string(), "type": "👍"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = read_json(resp).await;
    let arr = v.as_array().expect("array");
    assert_eq!(arr.len(), 1, "type filter で 1 件; got {arr:?}");
    assert_eq!(arr[0]["type"], "👍");
    assert_eq!(arr[0]["user"]["username"], "bob");
}

/// 存在しない note は 404 `NO_SUCH_NOTE`。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_reactions_unknown_note_404(pool: PgPool) {
    let _alice = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["read:account"]).await;
    let resp = post(
        app,
        "/api/notes/reactions",
        json!({"i": token, "noteId": "999999"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// token 無しは 401。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_reactions_without_token_401(pool: PgPool) {
    let alice = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let note_id = seed_note(&pool, alice, "sakurasato.test", "hi", Visibility::Public).await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let resp = post(
        app,
        "/api/notes/reactions",
        json!({"noteId": note_id.to_string()}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// `direct` visibility の他人 note の reaction は viewer に漏らさない (404)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_reactions_hidden_for_invisible_direct_note(pool: PgPool) {
    // local actor は resolve_self_actor_id のために必要だが id は使わない。
    let _alice = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let bob = seed_remote_actor(&pool, "remote.test", "bob").await;
    // bob の direct note (alice 宛でない = audience 空)。
    let note_id = seed_note_with_audience(
        &pool,
        bob,
        "remote.test",
        "secret",
        Visibility::Direct,
        vec![],
        vec![],
    )
    .await;
    repo::reaction::insert(&pool, "https://remote.test/r/1", note_id, bob, "👍", None)
        .await
        .unwrap();

    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["read:account"]).await;
    let resp = post(
        app,
        "/api/notes/reactions",
        json!({"i": token, "noteId": note_id.to_string()}),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "見えない direct note の reaction は 404"
    );
}

// ─── myReaction (リアクション増殖 fix) ───────────────────────────────────

/// viewer 自身が反応した note は `notes/show` で `myReaction` にその content を返し、
/// `reactions` map の同じ key と一致すること (= Aria が自分の反応を特定する不変条件)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_show_returns_my_reaction_when_viewer_reacted(pool: PgPool) {
    let alice = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let note_id = seed_note(&pool, alice, "sakurasato.test", "hi", Visibility::Public).await;
    repo::reaction::insert(
        &pool,
        "https://sakurasato.test/users/alice/r/1",
        note_id,
        alice,
        "👍",
        None,
    )
    .await
    .unwrap();
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["read:account"]).await;

    let resp = post(
        app,
        "/api/notes/show",
        json!({"i": token, "noteId": note_id.to_string()}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let note = read_json(resp).await;
    assert_eq!(note["myReaction"], "👍");
    assert!(
        note["reactions"]["👍"].is_number(),
        "myReaction は reactions map の key と一致する; got {:?}",
        note["reactions"]
    );
}

/// 反応していない note では `myReaction` は **present かつ null** (always-emit 担保)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_show_my_reaction_null_when_not_reacted(pool: PgPool) {
    let alice = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let note_id = seed_note(&pool, alice, "sakurasato.test", "hi", Visibility::Public).await;
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["read:account"]).await;

    let resp = post(
        app,
        "/api/notes/show",
        json!({"i": token, "noteId": note_id.to_string()}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let note = read_json(resp).await;
    assert!(
        note.get("myReaction").is_some(),
        "myReaction field は常に present (null 含む)"
    );
    assert!(note["myReaction"].is_null(), "反応していないので null");
}

/// ローカル custom emoji 反応は `myReaction == ":foo:"` で `reactions` key と一致。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn notes_show_my_reaction_local_custom_emoji(pool: PgPool) {
    let alice = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let note_id = seed_note(&pool, alice, "sakurasato.test", "hi", Visibility::Public).await;
    let emoji = repo::emoji::upsert_local(
        &pool,
        NewLocalEmoji {
            shortcode: "foo".into(),
            category: None,
            aliases: vec![],
            image_key: "emoji/local/foo.webp".into(),
            media_type: "image/webp".into(),
            license: None,
            is_sensitive: false,
        },
    )
    .await
    .expect("seed local emoji");
    repo::reaction::insert(
        &pool,
        "https://sakurasato.test/users/alice/r/1",
        note_id,
        alice,
        ":foo:",
        Some(emoji.id),
    )
    .await
    .unwrap();
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["read:account"]).await;

    let resp = post(
        app,
        "/api/notes/show",
        json!({"i": token, "noteId": note_id.to_string()}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let note = read_json(resp).await;
    assert_eq!(note["myReaction"], ":foo:");
    assert!(note["reactions"][":foo:"].is_number());
}

/// home timeline でも viewer 自身の反応が `myReaction` に乗る。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn timeline_returns_my_reaction_for_viewer_reaction(pool: PgPool) {
    let alice = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let note_id = seed_note(&pool, alice, "sakurasato.test", "hi", Visibility::Public).await;
    repo::reaction::insert(
        &pool,
        "https://sakurasato.test/users/alice/r/1",
        note_id,
        alice,
        "👍",
        None,
    )
    .await
    .unwrap();
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["read:account"]).await;

    let resp = post(app, "/api/notes/timeline", json!({"i": token, "limit": 10})).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let arr = read_json(resp).await;
    let notes = arr.as_array().expect("timeline array");
    let want_id = note_id.to_string();
    let n = notes
        .iter()
        .find(|n| n["id"] == want_id)
        .expect("note present in timeline");
    assert_eq!(n["myReaction"], "👍");
}

/// **viewer-scope の証明**: 別 actor だけが反応した note は viewer の `myReaction`
/// が null のまま (= 他人の反応を「自分の」と取り違えない)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn timeline_my_reaction_null_for_other_users_reaction_only(pool: PgPool) {
    let alice = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let bob = seed_remote_actor(&pool, "remote.test", "bob").await;
    let note_id = seed_note(&pool, alice, "sakurasato.test", "hi", Visibility::Public).await;
    // bob (= viewer ではない) だけが反応。
    repo::reaction::insert(&pool, "https://remote.test/r/1", note_id, bob, "👍", None)
        .await
        .unwrap();
    let state = make_state(pool.clone(), "sakurasato.test", "alice");
    let app = router_for(&state);
    let token = issue_token_with_scopes(&pool, &["read:account"]).await;

    let resp = post(app, "/api/notes/timeline", json!({"i": token, "limit": 10})).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let arr = read_json(resp).await;
    let notes = arr.as_array().expect("timeline array");
    let want_id = note_id.to_string();
    let n = notes
        .iter()
        .find(|n| n["id"] == want_id)
        .expect("note present in timeline");
    assert!(
        n["myReaction"].is_null(),
        "他人の反応は viewer の myReaction にならない; got {:?}",
        n["myReaction"]
    );
    assert_eq!(n["reactions"]["👍"], 1);
}
