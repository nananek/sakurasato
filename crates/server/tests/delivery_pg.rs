//! M3b-2 PR2 アウトバウンド配送の E2E テスト。
//!
//! - 実 RSA 鍵を持つ local actor を DB に seed
//! - tokio で受け側ダミー HTTP サーバを random port に立てる
//! - `delivery::enqueue_activity` → `delivery::try_deliver_one` の経路を
//!   完走させ、受け取った request の `Signature` / `Digest` / `Date` /
//!   `Host` を検証する
//!
//! 既存の `routes_pg.rs` と違って、こちらは **本物の HTTP を 1 ホップ撃つ**。
//! `reqwest::Client` の TLS / redirect / timeout 設定が間違っていないか、
//! 署名ヘッダが axum 側まで素通りするかをまとめて検証する。

#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use rsa::RsaPrivateKey;
use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
use rsa::rand_core::OsRng;
use sakurasato_core::repo;
use sakurasato_core::repo::actor::NewActor;
use sakurasato_server::delivery::{self, DeliveryOutcome};
use sakurasato_server::state::AppState;
use sqlx::PgPool;
use tokio::net::TcpListener;

#[derive(Default, Clone)]
struct CapturedRequest {
    headers: HeaderMap,
    body: Vec<u8>,
}

type Capture = Arc<Mutex<Option<CapturedRequest>>>;

async fn capture_handler(
    State(capture): State<Capture>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> StatusCode {
    *capture.lock().unwrap() = Some(CapturedRequest {
        headers,
        body: body.to_vec(),
    });
    StatusCode::ACCEPTED
}

/// Random port にダミー inbox サーバを立てる。返り値の URL を delivery
/// row の `inbox_url` に入れる。
async fn spawn_capture_server() -> (String, Capture) {
    let capture: Capture = Arc::new(Mutex::new(None));
    let app = Router::new()
        .route("/inbox", post(capture_handler))
        .with_state(capture.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/inbox"), capture)
}

fn fresh_rsa_pair() -> (String, String) {
    // 1024-bit はテスト高速化目的。プロダクションは init.rs が 2048-bit 使用。
    let sk = RsaPrivateKey::new(&mut OsRng, 1024).unwrap();
    let pk = sk.to_public_key();
    let priv_pem = sk.to_pkcs8_pem(LineEnding::LF).unwrap().to_string();
    let pub_pem = pk.to_public_key_pem(LineEnding::LF).unwrap();
    (priv_pem, pub_pem)
}

fn local_actor_with_real_key(username: &str, host: &str) -> NewActor {
    let ap_id = format!("https://{host}/users/{username}");
    let (priv_pem, pub_pem) = fresh_rsa_pair();
    NewActor {
        ap_id: ap_id.clone(),
        preferred_username: username.into(),
        host: host.into(),
        display_name: None,
        summary: None,
        icon_url: None,
        image_url: None,
        inbox_url: format!("{ap_id}/inbox"),
        shared_inbox_url: None,
        outbox_url: None,
        followers_url: None,
        following_url: None,
        public_key_id: format!("{ap_id}#main-key"),
        public_key_pem: pub_pem,
        private_key_pem: Some(priv_pem),
        ed25519_public_key_id: None,
        ed25519_public_key_pem: None,
        ed25519_private_key_pem: None,
        also_known_as: vec![],
        moved_to_ap_id: None,
        is_local: true,
        actor_type: "Person".into(),
    }
}

fn make_config(host: &str) -> sakurasato_core::Config {
    sakurasato_core::Config {
        server: sakurasato_core::config::ServerConfig {
            host: host.into(),
            bind: "127.0.0.1:0".into(),
            local_api_socket: "/tmp/sakurasato.sock".into(),
            user: "alice".into(),
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
async fn deliver_one_signs_and_succeeds(pool: PgPool) {
    let sender = repo::actor::insert(&pool, local_actor_with_real_key("alice", "example.test"))
        .await
        .unwrap();
    let (inbox_url, capture) = spawn_capture_server().await;

    let activity = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "type": "Create",
        "actor": sender.ap_id,
        "object": {"type": "Note", "content": "hello"},
    });
    let row = delivery::enqueue_activity(&pool, sender.id, &inbox_url, &activity)
        .await
        .unwrap();

    let state = AppState::from_pool(pool.clone(), make_config("example.test"));
    let outcome = delivery::try_deliver_one(&state, row.id).await.unwrap();
    assert_eq!(outcome, DeliveryOutcome::Delivered);

    // 受信側に届いたヘッダを検証。
    let captured = capture
        .lock()
        .unwrap()
        .clone()
        .expect("inbox server received no request");
    assert!(captured.headers.contains_key("signature"));
    assert!(captured.headers.contains_key("digest"));
    assert!(captured.headers.contains_key("date"));
    assert!(captured.headers.contains_key("host"));
    let ct = captured
        .headers
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        ct.starts_with("application/activity+json"),
        "Content-Type must be activity+json, got {ct}"
    );
    let sig = captured.headers.get("signature").unwrap().to_str().unwrap();
    assert!(
        sig.contains(&format!(r#"keyId="{}""#, sender.public_key_id)),
        "Signature keyId must match local actor's public_key_id: {sig}"
    );
    assert!(sig.contains(r#"algorithm="rsa-sha256""#), "{sig}");
    assert!(
        sig.contains(r#"headers="(request-target) host date digest""#),
        "{sig}"
    );

    // body も到達済み (digest 検証可能な完全な JSON)。
    let received: serde_json::Value = serde_json::from_slice(&captured.body).unwrap();
    assert_eq!(received["type"], "Create");

    // queue 行が delivered に倒れていること。
    let done = repo::delivery_queue::get_by_id(&pool, row.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(done.state, "delivered");
    assert_eq!(done.attempts, 1);
    assert!(done.last_error.is_none());
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn deliver_one_marks_retry_on_non_2xx(pool: PgPool) {
    // 受け側が常に 500 を返すなら retry に倒り、queue 行は failed 状態 +
    // last_error 記録 + attempts インクリメント。
    let sender = repo::actor::insert(&pool, local_actor_with_real_key("bob", "example.test"))
        .await
        .unwrap();
    let app = Router::new().route(
        "/inbox",
        post(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let inbox_url = format!("http://{addr}/inbox");

    let row = delivery::enqueue_activity(
        &pool,
        sender.id,
        &inbox_url,
        &serde_json::json!({"type": "Create"}),
    )
    .await
    .unwrap();

    let state = AppState::from_pool(pool.clone(), make_config("example.test"));
    let outcome = delivery::try_deliver_one(&state, row.id).await.unwrap();
    assert_eq!(outcome, DeliveryOutcome::Retry);

    let after = repo::delivery_queue::get_by_id(&pool, row.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.state, "failed");
    assert_eq!(after.attempts, 1);
    assert!(
        after.last_error.as_deref().unwrap().contains("HTTP 500"),
        "last_error must record the HTTP status: {:?}",
        after.last_error
    );
}
