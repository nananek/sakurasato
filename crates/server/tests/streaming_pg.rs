//! Misskey 互換 `/streaming` (Aria 等) の **broadcast 購読レベル** 統合テスト
//! (親 #150 / #170)。
//!
//! WebSocket を実際に張らず、`AppState::stream_sender().subscribe()` を購読して、
//! ローカル API 経由の実 HTTP ハンドラ (note 作成 / reaction 作成・削除) が
//! [`sakurasato_server::event_bus::StreamEvent`] を発火することを E2E で確認する。
//! `from_pool` 経由なので versitygw / media-proxy には到達しない。
//!
//! WebSocket frame への変換 (channel routing / envelope) は
//! `crate::miauth::streaming` の純関数ユニットテストが担う ── ここは
//! 「publisher が正しいイベントを bus に流す」配線だけを検証する。

#![forbid(unsafe_code)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use sakurasato_core::model::Visibility;
use sakurasato_core::repo;
use sakurasato_core::repo::actor::NewActor;
use sakurasato_server::event_bus::{ReactionKind, StreamEvent};
use sakurasato_server::state::AppState;
use sqlx::PgPool;
use tokio::sync::broadcast::Receiver;
use tower::ServiceExt;

const HOST: &str = "example.test";
const USER: &str = "alice";

fn sample_local_actor() -> NewActor {
    let ap_id = format!("https://{HOST}/users/{USER}");
    NewActor {
        ap_id: ap_id.clone(),
        preferred_username: USER.into(),
        host: HOST.into(),
        display_name: Some("Alice".into()),
        summary: None,
        icon_url: None,
        image_url: None,
        inbox_url: format!("{ap_id}/inbox"),
        shared_inbox_url: Some(format!("https://{HOST}/inbox")),
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
    }
}

fn make_config() -> sakurasato_core::Config {
    sakurasato_core::Config {
        server: sakurasato_core::config::ServerConfig {
            host: HOST.into(),
            bind: "127.0.0.1:0".into(),
            local_api_socket: "/tmp/sakurasato.sock".into(),
            public_listen: None,
            local_api_listen: None,
            user: USER.into(),
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
            video: sakurasato_core::config::VideoConfig::default(),
            emoji_import: sakurasato_core::config::EmojiImportConfig::default(),
        },
        miauth: None,
    }
}

async fn issue_token(pool: &PgPool) -> String {
    let raw = sakurasato_server::token::generate_raw();
    let hash = sakurasato_server::token::hash(&raw);
    repo::api_token::insert(
        pool,
        sakurasato_core::repo::api_token::NewApiToken {
            name: "tui".into(),
            token_hash: hash,
        },
    )
    .await
    .unwrap();
    raw
}

async fn seed_note(pool: &PgPool, actor_id: i64) -> i64 {
    let ap_id = format!("https://{HOST}/notes/seed");
    repo::note::insert(
        pool,
        repo::note::NewNote {
            ap_id: ap_id.clone(),
            actor_id,
            content: "hi".into(),
            language: None,
            in_reply_to_ap_id: None,
            in_reply_to_note_id: None,
            summary: None,
            visibility: Visibility::Public,
            sensitive: false,
            to_recipients: vec![],
            cc_recipients: vec![],
            attachments: serde_json::json!([]),
            tags: serde_json::json!([]),
            is_local: true,
            url: Some(ap_id),
            published_at: chrono::Utc::now(),
        },
    )
    .await
    .unwrap()
    .id
}

async fn read_json(resp: axum::response::Response) -> serde_json::Value {
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap()
}

/// 購読チャンネルから最初の [`StreamEvent`] を非ブロッキングで取り出す。
/// publisher は HTTP ハンドラ内で同期 `send` するので、レスポンス受領後は
/// 既に bus に載っている。
fn recv_event(rx: &mut Receiver<StreamEvent>) -> StreamEvent {
    rx.try_recv()
        .expect("expected a StreamEvent on the streaming bus")
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn local_note_create_publishes_note_event(pool: PgPool) {
    repo::actor::insert(&pool, sample_local_actor())
        .await
        .unwrap();
    let raw = issue_token(&pool).await;
    let state = AppState::from_pool(pool.clone(), make_config());
    let mut rx = state.stream_sender().subscribe();
    let app = sakurasato_server::local_api::router(state);

    let body = serde_json::json!({"content": "hello streaming", "visibility": "public"});
    let resp = app
        .oneshot(
            Request::post("/api/v1/notes")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = read_json(resp).await;
    let created_id = json["id"].as_i64().unwrap();

    match recv_event(&mut rx) {
        StreamEvent::Note { note_id } => assert_eq!(note_id, created_id),
        other => panic!("expected StreamEvent::Note, got {other:?}"),
    }
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn local_reaction_create_publishes_reacted_event(pool: PgPool) {
    let actor = repo::actor::insert(&pool, sample_local_actor())
        .await
        .unwrap();
    let note_id = seed_note(&pool, actor.id).await;
    let raw = issue_token(&pool).await;
    let state = AppState::from_pool(pool.clone(), make_config());
    let mut rx = state.stream_sender().subscribe();
    let app = sakurasato_server::local_api::router(state);

    let body = serde_json::json!({"note_id": note_id, "content": "👍"});
    let resp = app
        .oneshot(
            Request::post("/api/v1/reactions")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    match recv_event(&mut rx) {
        StreamEvent::ReactionUpdated {
            note_id: n,
            reaction,
            kind,
        } => {
            assert_eq!(n, note_id);
            assert_eq!(reaction, "👍");
            assert_eq!(kind, ReactionKind::Reacted);
        }
        other => panic!("expected StreamEvent::ReactionUpdated(Reacted), got {other:?}"),
    }
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn local_reaction_delete_publishes_unreacted_event(pool: PgPool) {
    let actor = repo::actor::insert(&pool, sample_local_actor())
        .await
        .unwrap();
    let note_id = seed_note(&pool, actor.id).await;
    let raw = issue_token(&pool).await;
    let state = AppState::from_pool(pool.clone(), make_config());
    let mut rx = state.stream_sender().subscribe();
    let app = sakurasato_server::local_api::router(state);

    // 作成 (Reacted イベントが 1 件流れる)。
    let body = serde_json::json!({"note_id": note_id, "content": "👍"});
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/v1/reactions")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let reaction_id = read_json(resp).await["id"].as_i64().unwrap();
    // Reacted を読み捨てる。
    assert!(matches!(
        recv_event(&mut rx),
        StreamEvent::ReactionUpdated {
            kind: ReactionKind::Reacted,
            ..
        }
    ));

    // 削除 (Unreacted イベント)。
    let resp = app
        .oneshot(
            Request::delete(format!("/api/v1/reactions/{reaction_id}"))
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    match recv_event(&mut rx) {
        StreamEvent::ReactionUpdated {
            note_id: n,
            reaction,
            kind,
        } => {
            assert_eq!(n, note_id);
            assert_eq!(reaction, "👍");
            assert_eq!(kind, ReactionKind::Unreacted);
        }
        other => panic!("expected StreamEvent::ReactionUpdated(Unreacted), got {other:?}"),
    }
}
