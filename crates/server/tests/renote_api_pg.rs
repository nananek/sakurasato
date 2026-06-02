//! #151 統合テスト: `POST /api/v1/notes/{id}/renote` /
//! `DELETE /api/v1/notes/{id}/renote`。
//!
//! `reaction_api_pg.rs` のパターンに揃え、本物の token + 本物の Note 行に対して
//! ローカル user が boost / undo-boost する経路を検証する。

#![forbid(unsafe_code)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use sakurasato_core::model::Visibility;
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
            manually_approves_followers: false,
        }
    }

    /// renote 対象 (= ローカル user 以外の note 作者) 用のリモート actor。
    /// PR #155 review Finding 1 で自己 renote が 422 になったので、各テストの
    /// 「renote 可能な note」は本 actor が author になる構成にする。
    pub(super) fn sample_remote_actor(username: &str, host: &str) -> NewActor {
        let ap_id = format!("https://{host}/users/{username}");
        NewActor {
            ap_id: ap_id.clone(),
            preferred_username: username.into(),
            host: host.into(),
            display_name: Some("Bob".into()),
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
            // remote actor は private_key を持たない。
            private_key_pem: None,
            ed25519_public_key_id: None,
            ed25519_public_key_pem: None,
            ed25519_private_key_pem: None,
            also_known_as: vec![],
            moved_to_ap_id: None,
            is_local: false,
            actor_type: "Person".into(),
            manually_approves_followers: false,
        }
    }

    fn sample_ed25519_public_pem() -> String {
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

async fn seed_note_with_visibility(
    pool: &PgPool,
    actor_id: i64,
    host: &str,
    visibility: Visibility,
    is_local: bool,
) -> i64 {
    let ap_id = format!(
        "https://{host}/notes/{seq}",
        seq = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(1)
    );
    let inserted = repo::note::insert(
        pool,
        repo::note::NewNote {
            ap_id: ap_id.clone(),
            actor_id,
            content: "hi".into(),
            language: None,
            in_reply_to_ap_id: None,
            in_reply_to_note_id: None,
            summary: None,
            visibility,
            sensitive: false,
            to_recipients: vec![],
            cc_recipients: vec![],
            attachments: serde_json::json!([]),
            tags: serde_json::json!([]),
            is_local,
            url: Some(ap_id),
            published_at: chrono::Utc::now(),
        },
    )
    .await
    .unwrap();
    inserted.id
}

/// 各テストの定番セットアップ: alice (local) と bob (remote) を作り、bob の
/// 指定 visibility note を 1 件 seed する。renote 経路は「他人の note を boost」
/// が前提なので bob.id を author に使う。
async fn seed_alice_and_bob_note(pool: &PgPool, visibility: Visibility) -> (i64, i64) {
    let alice = repo::actor::insert(pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let bob = repo::actor::insert(pool, common::sample_remote_actor("bob", "remote.test"))
        .await
        .unwrap();
    let note_id = seed_note_with_visibility(pool, bob.id, "remote.test", visibility, false).await;
    (alice.id, note_id)
}

async fn read_json(resp: axum::response::Response) -> serde_json::Value {
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap()
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn create_renote_inserts_announce_row(pool: PgPool) {
    let (alice_id, note_id) = seed_alice_and_bob_note(&pool, Visibility::Public).await;
    let raw = issue_token(&pool, "tui").await;
    let state =
        sakurasato_server::state::AppState::from_pool(pool.clone(), make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = app
        .oneshot(
            Request::post(format!("/api/v1/notes/{note_id}/renote"))
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = read_json(resp).await;
    assert_eq!(json["note_id"], note_id);
    let ap_id = json["ap_id"].as_str().unwrap();
    assert!(
        ap_id.starts_with("https://example.test/users/alice/activities/announce-"),
        "ap_id {ap_id} should start with announce- pattern",
    );
    // followers 0 件、bob (remote) の shared_inbox に 1 件 enqueue される。
    assert_eq!(json["queued_deliveries"], 1);

    // DB 確認。
    let row = repo::announce::get_by_pair(&pool, note_id, alice_id)
        .await
        .unwrap();
    assert!(row.is_some());
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn create_renote_unlisted_is_allowed(pool: PgPool) {
    let (_alice_id, note_id) = seed_alice_and_bob_note(&pool, Visibility::Unlisted).await;
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = app
        .oneshot(
            Request::post(format!("/api/v1/notes/{note_id}/renote"))
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn create_renote_rejects_followers_visibility(pool: PgPool) {
    let (_alice_id, note_id) = seed_alice_and_bob_note(&pool, Visibility::Followers).await;
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = app
        .oneshot(
            Request::post(format!("/api/v1/notes/{note_id}/renote"))
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn create_renote_rejects_direct_visibility(pool: PgPool) {
    let (_alice_id, note_id) = seed_alice_and_bob_note(&pool, Visibility::Direct).await;
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = app
        .oneshot(
            Request::post(format!("/api/v1/notes/{note_id}/renote"))
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// PR #155 review Finding 1: 自己 renote (= local actor が自分の note を
/// renote) は 422 で拒否される。Mastodon / Misskey 互換。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn create_renote_rejects_self_renote(pool: PgPool) {
    let alice = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    // alice 自身の public note を seed。
    let note_id =
        seed_note_with_visibility(&pool, alice.id, "example.test", Visibility::Public, true).await;
    let raw = issue_token(&pool, "tui").await;
    let state =
        sakurasato_server::state::AppState::from_pool(pool.clone(), make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = app
        .oneshot(
            Request::post(format!("/api/v1/notes/{note_id}/renote"))
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    // DB に announce 行を残してはいけない。
    let row = repo::announce::get_by_pair(&pool, note_id, alice.id)
        .await
        .unwrap();
    assert!(row.is_none(), "self-renote must not insert announce row");
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn create_renote_duplicate_is_idempotent(pool: PgPool) {
    let (_alice_id, note_id) = seed_alice_and_bob_note(&pool, Visibility::Public).await;
    let raw = issue_token(&pool, "tui").await;
    let state =
        sakurasato_server::state::AppState::from_pool(pool.clone(), make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let first = app
        .clone()
        .oneshot(
            Request::post(format!("/api/v1/notes/{note_id}/renote"))
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let first_json = read_json(first).await;
    let first_id = first_json["id"].as_i64().unwrap();

    let second = app
        .oneshot(
            Request::post(format!("/api/v1/notes/{note_id}/renote"))
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::CREATED);
    let second_json = read_json(second).await;
    assert_eq!(second_json["id"], first_id, "same announce row returned");
    assert_eq!(second_json["queued_deliveries"], 0, "no re-delivery");
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn delete_renote_removes_announce_row(pool: PgPool) {
    let (alice_id, note_id) = seed_alice_and_bob_note(&pool, Visibility::Public).await;
    let raw = issue_token(&pool, "tui").await;
    let state =
        sakurasato_server::state::AppState::from_pool(pool.clone(), make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    // POST → DELETE。
    let _ = app
        .clone()
        .oneshot(
            Request::post(format!("/api/v1/notes/{note_id}/renote"))
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let resp = app
        .oneshot(
            Request::delete(format!("/api/v1/notes/{note_id}/renote"))
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 行が消えている。
    let gone = repo::announce::get_by_pair(&pool, note_id, alice_id)
        .await
        .unwrap();
    assert!(gone.is_none(), "announce row must be deleted");
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn delete_renote_not_found_returns_404(pool: PgPool) {
    let (_alice_id, note_id) = seed_alice_and_bob_note(&pool, Visibility::Public).await;
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    let resp = app
        .oneshot(
            Request::delete(format!("/api/v1/notes/{note_id}/renote"))
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn home_timeline_reflects_viewer_renoted(pool: PgPool) {
    // alice (local) と bob (remote) を作り、alice が bob を follow している
    // 状態にする (= bob の public note が alice の home TL に乗る前提)。
    let alice = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let bob = repo::actor::insert(&pool, common::sample_remote_actor("bob", "remote.test"))
        .await
        .unwrap();
    let follow = repo::follow::insert_pending(
        &pool,
        "https://example.test/users/alice/follows/bob",
        alice.id,
        bob.id,
    )
    .await
    .unwrap();
    repo::follow::set_state(
        &pool,
        follow.id,
        sakurasato_core::model::FollowState::Accepted,
    )
    .await
    .unwrap();
    let note_id =
        seed_note_with_visibility(&pool, bob.id, "remote.test", Visibility::Public, false).await;
    let raw = issue_token(&pool, "tui").await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::local_api::router(state);

    // POST renote。
    let _ = app
        .clone()
        .oneshot(
            Request::post(format!("/api/v1/notes/{note_id}/renote"))
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // home timeline で `announce_count = 1` / `viewer_renoted = true` を確認。
    let resp = app
        .oneshot(
            Request::get("/api/v1/timeline/home")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    let notes = json["notes"].as_array().unwrap();
    let target = notes
        .iter()
        .find(|n| n["id"].as_i64() == Some(note_id))
        .expect("target note not in timeline");
    assert_eq!(target["announce_count"], 1);
    assert_eq!(target["viewer_renoted"], true);
}
