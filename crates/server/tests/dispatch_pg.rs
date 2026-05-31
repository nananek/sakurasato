//! M3b-3 PR2 統合テスト: 受信 inbox に Follow が来てから Accept が
//! `delivery_queue` に積まれるまでの dispatch 経路を E2E で検証する。
//!
//! `routes_pg.rs` が `WebFinger` / `actor` の単体エンドポイントを叩く
//! のに対し、こちらは「署名検証 → F3 actor 一致 → handler → 配送 enqueue」
//! という縦の経路全体を、本物の RSA 鍵 + 実 Postgres で通す。
//!
//! ## カバー範囲
//!
//! - **Follow 受領** → `follow` 行が **accepted で入る** (お一人様 + 自動承認
//!   設計) + `delivery_queue` に Accept activity が積まれる + Accept の
//!   `object` が元 Follow を埋め込む + `list_accepted_inboxes` に follower の
//!   inbox が即時現れる (= こちらの Note を配送できる状態)
//! - **Accept 受領** → 既存 follow 行が accepted に遷移 (outbound Follow の
//!   応答処理)
//! - **F3 actor mismatch 拒否** → 401 (受信 inbox に到達後、handler 前で弾く)
//! - **delivery worker 常駐ループ** → enqueue した行が拾われ、配送先 HTTP
//!   サーバが POST を受け取り、delivered に倒れる
//!
//! 受信側 inbox は loopback で立てる ── テスト経路 `AppState::from_pool`
//! が SSRF ガードを緩めるフラグを持つので動作する。

#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::routing::post;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use rsa::RsaPrivateKey;
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey, EncodePublicKey, LineEnding};
use rsa::rand_core::OsRng;
use rsa::signature::{SignatureEncoding, SignerMut};
use sakurasato_core::repo;
use sakurasato_core::repo::actor::NewActor;
use sakurasato_server::routes::router;
use sakurasato_server::state::AppState;
use sqlx::PgPool;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tower::ServiceExt;

const LOCAL_HOST: &str = "sakura.test";
const LOCAL_USER: &str = "alice";

fn make_config() -> sakurasato_core::Config {
    sakurasato_core::Config {
        server: sakurasato_core::config::ServerConfig {
            host: LOCAL_HOST.into(),
            bind: "127.0.0.1:0".into(),
            local_api_socket: "/tmp/sakurasato.sock".into(),
            user: LOCAL_USER.into(),
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

fn fresh_rsa() -> (String, String) {
    let sk = RsaPrivateKey::new(&mut OsRng, 1024).unwrap();
    let pk = sk.to_public_key();
    let priv_pem = sk.to_pkcs8_pem(LineEnding::LF).unwrap().to_string();
    let pub_pem = pk.to_public_key_pem(LineEnding::LF).unwrap();
    (priv_pem, pub_pem)
}

fn local_actor(pub_pem: &str, priv_pem: &str) -> NewActor {
    let ap_id = format!("https://{LOCAL_HOST}/users/{LOCAL_USER}");
    NewActor {
        ap_id: ap_id.clone(),
        preferred_username: LOCAL_USER.into(),
        host: LOCAL_HOST.into(),
        display_name: None,
        summary: None,
        icon_url: None,
        image_url: None,
        inbox_url: format!("{ap_id}/inbox"),
        shared_inbox_url: Some(format!("https://{LOCAL_HOST}/inbox")),
        outbox_url: Some(format!("{ap_id}/outbox")),
        followers_url: None,
        following_url: None,
        public_key_id: format!("{ap_id}#main-key"),
        public_key_pem: pub_pem.into(),
        private_key_pem: Some(priv_pem.into()),
        ed25519_public_key_id: None,
        ed25519_public_key_pem: None,
        ed25519_private_key_pem: None,
        also_known_as: vec![],
        moved_to_ap_id: None,
        is_local: true,
        actor_type: "Person".into(),
    }
}

fn remote_actor(host: &str, user: &str, pub_pem: &str, inbox_url: &str) -> NewActor {
    let ap_id = format!("https://{host}/users/{user}");
    NewActor {
        ap_id: ap_id.clone(),
        preferred_username: user.into(),
        host: host.into(),
        display_name: None,
        summary: None,
        icon_url: None,
        image_url: None,
        inbox_url: inbox_url.into(),
        shared_inbox_url: None,
        outbox_url: None,
        followers_url: None,
        following_url: None,
        public_key_id: format!("{ap_id}#main-key"),
        public_key_pem: pub_pem.into(),
        private_key_pem: None,
        ed25519_public_key_id: None,
        ed25519_public_key_pem: None,
        ed25519_private_key_pem: None,
        also_known_as: vec![],
        moved_to_ap_id: None,
        is_local: false,
        actor_type: "Person".into(),
    }
}

#[derive(Default, Clone)]
struct CapturedRequest {
    #[allow(dead_code)] // headers は将来のテストで参照する想定
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

/// cavage RSA-SHA256 で署名した POST request を組み立てる。テスト用の最小実装。
fn build_signed_post(
    body: &[u8],
    path: &str,
    priv_pem: &str,
    keyid: &str,
    host: &str,
) -> Request<Body> {
    let date = httpdate::fmt_http_date(std::time::SystemTime::now());
    let digest_value = format!(
        "SHA-256={}",
        B64.encode(<sha2::Sha256 as sha2::Digest>::digest(body))
    );

    let mut headers = http::HeaderMap::new();
    headers.insert("host", http::HeaderValue::from_str(host).unwrap());
    headers.insert("date", http::HeaderValue::from_str(&date).unwrap());
    headers.insert(
        "digest",
        http::HeaderValue::from_str(&digest_value).unwrap(),
    );

    // signature base: cavage 形式
    let covered = ["(request-target)", "host", "date", "digest"];
    let mut base_lines = Vec::new();
    for h in &covered {
        match *h {
            "(request-target)" => base_lines.push(format!("(request-target): post {path}")),
            other => {
                let v = headers.get(other).unwrap().to_str().unwrap();
                base_lines.push(format!("{other}: {v}"));
            }
        }
    }
    let base = base_lines.join("\n");

    // RSA-SHA256 で署名
    let priv_key = RsaPrivateKey::from_pkcs8_pem(priv_pem).unwrap();
    let mut signer = SigningKey::<sha2::Sha256>::new(priv_key);
    let sig = signer.sign(base.as_bytes());
    let sig_b64 = B64.encode(sig.to_bytes());

    let sig_header = format!(
        r#"keyId="{keyid}",algorithm="rsa-sha256",headers="{}",signature="{sig_b64}""#,
        covered.join(" "),
    );

    Request::post(path)
        .header("host", host)
        .header("date", date)
        .header("digest", digest_value)
        .header("content-type", "application/activity+json")
        .header("signature", sig_header)
        .body(Body::from(body.to_vec()))
        .unwrap()
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn follow_request_enqueues_accept(pool: PgPool) {
    let (local_priv, local_pub) = fresh_rsa();
    let (remote_priv, remote_pub) = fresh_rsa();
    let _ = local_priv;

    let local = repo::actor::insert(&pool, local_actor(&local_pub, "PRIV"))
        .await
        .unwrap();
    let remote_inbox = "https://remote.test/users/bob/inbox".to_string();
    let remote = repo::actor::insert(
        &pool,
        remote_actor("remote.test", "bob", &remote_pub, &remote_inbox),
    )
    .await
    .unwrap();

    let state = AppState::from_pool(pool.clone(), make_config());
    let app = router(state);

    // remote から Follow を投げ込む。署名は remote の秘密鍵。
    let follow_id = format!(
        "https://remote.test/users/bob/activities/follow-{}",
        local.id
    );
    let body = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": follow_id,
        "type": "Follow",
        "actor": remote.ap_id,
        "object": local.ap_id,
    })
    .to_string();
    let keyid = format!("{}#main-key", remote.ap_id);
    let req = build_signed_post(body.as_bytes(), "/inbox", &remote_priv, &keyid, LOCAL_HOST);

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "Follow must be accepted"
    );

    // follow row は **即 accepted**。お一人様 + 自動承認設計なので Accept を
    // queue した時点で state を倒す (handle_follow)。pending のままだと
    // `repo::follow::list_accepted_inboxes` から外れ、こちらからの Note /
    // reaction が一切配送されない (連合テストで露見した既存バグの再発防止)。
    let follow = sqlx::query!(
        "SELECT id, ap_id, follower_actor_id, followed_actor_id, state FROM follow WHERE ap_id = $1",
        follow_id,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(follow.state, "accepted");
    assert_eq!(follow.follower_actor_id, remote.id);
    assert_eq!(follow.followed_actor_id, local.id);

    // 即時 accepted の効果: follower の inbox が配送先として列挙される。
    let inboxes = repo::follow::list_accepted_inboxes(&pool, local.id)
        .await
        .unwrap();
    assert!(
        inboxes.iter().any(|u| u == &remote_inbox),
        "follower inbox {remote_inbox} must be a delivery target, got {inboxes:?}",
    );

    // delivery_queue に Accept が 1 行積まれた。
    let queued = sqlx::query!(
        r#"SELECT id, inbox_url, activity as "activity: sqlx::types::Json<serde_json::Value>",
              sender_actor_id, state
          FROM delivery_queue WHERE sender_actor_id = $1 AND state = 'pending'"#,
        local.id,
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(queued.len(), 1, "Accept must be queued for delivery");
    let row = &queued[0];
    assert_eq!(row.inbox_url, remote_inbox);

    // Accept activity の中身を検証: actor は local、object は元 Follow を埋め込み。
    let activity = &row.activity.0;
    assert_eq!(activity["type"], "Accept");
    assert_eq!(activity["actor"], local.ap_id);
    let accept_id = activity["id"].as_str().unwrap();
    assert!(
        accept_id.starts_with(&format!(
            "https://{LOCAL_HOST}/users/{LOCAL_USER}/activities/accept-"
        )),
        "Accept id is deterministic per follow row: {accept_id}",
    );
    let object = &activity["object"];
    assert_eq!(object["id"], follow_id);
    assert_eq!(object["actor"], remote.ap_id);
    assert_eq!(object["object"], local.ap_id);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn duplicate_follow_is_idempotent(pool: PgPool) {
    // 同じ Follow が二度届いても follow 行は 1 つ。Accept は 2 つ積まれる
    // (相手の retry を考えると Accept を再送するのは妥当)。
    let (local_priv, local_pub) = fresh_rsa();
    let (remote_priv, remote_pub) = fresh_rsa();
    let _ = local_priv;

    let local = repo::actor::insert(&pool, local_actor(&local_pub, "PRIV"))
        .await
        .unwrap();
    let remote = repo::actor::insert(
        &pool,
        remote_actor(
            "remote.test",
            "bob",
            &remote_pub,
            "https://remote.test/users/bob/inbox",
        ),
    )
    .await
    .unwrap();

    let state = AppState::from_pool(pool.clone(), make_config());
    let app1 = router(state.clone());
    let app2 = router(state);

    let follow_id = format!("https://remote.test/users/bob/activities/dupe-{}", local.id);
    let body = serde_json::json!({
        "id": follow_id,
        "type": "Follow",
        "actor": remote.ap_id,
        "object": local.ap_id,
    })
    .to_string();
    let keyid = format!("{}#main-key", remote.ap_id);

    for app in [app1, app2] {
        let req = build_signed_post(body.as_bytes(), "/inbox", &remote_priv, &keyid, LOCAL_HOST);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
    }

    let row = sqlx::query!(
        "SELECT count(*) as c, max(state) as state FROM follow WHERE ap_id = $1",
        follow_id,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.c.unwrap_or(0), 1, "follow row must not duplicate");
    assert_eq!(
        row.state.as_deref(),
        Some("accepted"),
        "duplicate Follow must keep state at accepted, not flip back to pending",
    );
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn accept_response_marks_follow_accepted(pool: PgPool) {
    // 我々が Follow した remote actor が Accept を返してきたケース。
    // 既存 pending follow → accepted に遷移する。
    let (local_priv, local_pub) = fresh_rsa();
    let (remote_priv, remote_pub) = fresh_rsa();
    let _ = local_priv;

    let local = repo::actor::insert(&pool, local_actor(&local_pub, "PRIV"))
        .await
        .unwrap();
    let remote = repo::actor::insert(
        &pool,
        remote_actor(
            "remote.test",
            "bob",
            &remote_pub,
            "https://remote.test/users/bob/inbox",
        ),
    )
    .await
    .unwrap();

    // 既存 pending follow (local → remote)。
    let follow_ap_id = format!("https://{LOCAL_HOST}/users/{LOCAL_USER}/activities/follow-test");
    let follow = repo::follow::insert_pending(&pool, &follow_ap_id, local.id, remote.id)
        .await
        .unwrap();

    let state = AppState::from_pool(pool.clone(), make_config());
    let app = router(state);

    // remote から Accept を投げ込む。object は文字列 (= Follow URI)。
    let body = serde_json::json!({
        "id": "https://remote.test/users/bob/activities/accept-1",
        "type": "Accept",
        "actor": remote.ap_id,
        "object": follow_ap_id,
    })
    .to_string();
    let keyid = format!("{}#main-key", remote.ap_id);
    let req = build_signed_post(body.as_bytes(), "/inbox", &remote_priv, &keyid, LOCAL_HOST);

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let updated = repo::follow::get_by_ap_id(&pool, &follow_ap_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.state, "accepted");
    assert_eq!(updated.id, follow.id);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn accept_from_unrelated_actor_is_rejected(pool: PgPool) {
    // 関係ない actor から Accept が来たら 401 (`DispatchError::UnrelatedAcceptor`)。
    // F3 を通っていても、followed actor と signer の紐付けが取れないものは
    // なりすまし試行として拒否する。503 ではなく 401 を返すことで、悪意ある
    // actor の Mastodon 系再送ループに乗らずに済む (round-2 review F3)。
    let (local_priv, local_pub) = fresh_rsa();
    let (remote_priv, remote_pub) = fresh_rsa();
    let (evil_priv, evil_pub) = fresh_rsa();
    let _ = local_priv;
    let _ = remote_priv;

    let local = repo::actor::insert(&pool, local_actor(&local_pub, "PRIV"))
        .await
        .unwrap();
    let remote = repo::actor::insert(
        &pool,
        remote_actor(
            "remote.test",
            "bob",
            &remote_pub,
            "https://remote.test/users/bob/inbox",
        ),
    )
    .await
    .unwrap();
    let evil = repo::actor::insert(
        &pool,
        remote_actor(
            "evil.test",
            "eve",
            &evil_pub,
            "https://evil.test/users/eve/inbox",
        ),
    )
    .await
    .unwrap();
    let _ = evil;

    let follow_ap_id =
        format!("https://{LOCAL_HOST}/users/{LOCAL_USER}/activities/follow-target-remote");
    let _ = repo::follow::insert_pending(&pool, &follow_ap_id, local.id, remote.id)
        .await
        .unwrap();

    let state = AppState::from_pool(pool.clone(), make_config());
    let app = router(state);

    let body = serde_json::json!({
        "id": "https://evil.test/users/eve/activities/fake-accept",
        "type": "Accept",
        "actor": "https://evil.test/users/eve",
        "object": follow_ap_id,
    })
    .to_string();
    let keyid = "https://evil.test/users/eve#main-key";
    let req = build_signed_post(body.as_bytes(), "/inbox", &evil_priv, keyid, LOCAL_HOST);

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "Accept by unrelated actor must be rejected as auth failure (not 5xx)",
    );

    // follow は依然 pending のまま。
    let unchanged = repo::follow::get_by_ap_id(&pool, &follow_ap_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unchanged.state, "pending");
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn f3_spoofed_body_actor_is_rejected_401(pool: PgPool) {
    // 署名は remote bob、body の actor は良い人 carol を装う ──
    // F3 ([`dispatch::verify_body_actor`]) で 401 拒否されることを確認。
    let (_priv, pub_pem) = fresh_rsa();
    let (remote_priv, remote_pub) = fresh_rsa();
    let _ = pub_pem;
    let _ = repo::actor::insert(
        &pool,
        remote_actor(
            "remote.test",
            "bob",
            &remote_pub,
            "https://remote.test/users/bob/inbox",
        ),
    )
    .await
    .unwrap();

    let state = AppState::from_pool(pool.clone(), make_config());
    let app = router(state);

    let body = serde_json::json!({
        "type": "Announce",
        "actor": "https://good.test/users/carol",
    })
    .to_string();
    let keyid = "https://remote.test/users/bob#main-key";
    let req = build_signed_post(body.as_bytes(), "/inbox", &remote_priv, keyid, LOCAL_HOST);

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "F3 body-actor / signer mismatch must be 401",
    );
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn follow_id_host_mismatch_is_rejected(pool: PgPool) {
    // round-2 F2 回帰: 署名は remote bob、Follow.id は good.example の URL。
    // F3 で body actor は一致 (= remote bob) を通すが、handler の host 一致
    // チェックで弾く。これを忘れると `evil.example` の有効署名者が任意の
    // `good.example/activities/...` を follow_ap_id として DB に混入できる。
    let (_priv, local_pub) = fresh_rsa();
    let (remote_priv, remote_pub) = fresh_rsa();
    let local = repo::actor::insert(&pool, local_actor(&local_pub, "PRIV"))
        .await
        .unwrap();
    let remote = repo::actor::insert(
        &pool,
        remote_actor(
            "remote.test",
            "bob",
            &remote_pub,
            "https://remote.test/users/bob/inbox",
        ),
    )
    .await
    .unwrap();

    let state = AppState::from_pool(pool.clone(), make_config());
    let app = router(state);

    // 署名は remote bob、body の actor も bob (F3 は通る)、
    // しかし activity id は good.example のホスト。
    let spoofed_follow_id = "https://good.example/activities/follow-9999";
    let body = serde_json::json!({
        "id": spoofed_follow_id,
        "type": "Follow",
        "actor": remote.ap_id,
        "object": local.ap_id,
    })
    .to_string();
    let keyid = format!("{}#main-key", remote.ap_id);
    let req = build_signed_post(body.as_bytes(), "/inbox", &remote_priv, &keyid, LOCAL_HOST);

    let resp = app.oneshot(req).await.unwrap();
    // anyhow Err 経由なので 503。重要なのは「200/202 にならず DB に行が入らない」こと。
    assert!(
        resp.status().is_server_error() || resp.status().is_client_error(),
        "spoofed Follow id must be rejected, got {}",
        resp.status(),
    );

    // follow 行は作られていない。
    let count = sqlx::query!(
        "SELECT count(*) as c FROM follow WHERE ap_id = $1",
        spoofed_follow_id,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        count.c.unwrap_or(0),
        0,
        "spoofed Follow id must not be inserted",
    );
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn worker_drains_pending_rows(pool: PgPool) {
    // worker::run を tokio::spawn で立ち上げて、enqueue した行が
    // 1 サイクル以内に delivered に倒れることを確認する。
    let (priv_pem, pub_pem) = fresh_rsa();
    let local = repo::actor::insert(
        &pool,
        NewActor {
            ap_id: format!("https://{LOCAL_HOST}/users/{LOCAL_USER}"),
            preferred_username: LOCAL_USER.into(),
            host: LOCAL_HOST.into(),
            display_name: None,
            summary: None,
            icon_url: None,
            image_url: None,
            inbox_url: format!("https://{LOCAL_HOST}/users/{LOCAL_USER}/inbox"),
            shared_inbox_url: None,
            outbox_url: None,
            followers_url: None,
            following_url: None,
            public_key_id: format!("https://{LOCAL_HOST}/users/{LOCAL_USER}#main-key"),
            public_key_pem: pub_pem,
            private_key_pem: Some(priv_pem),
            ed25519_public_key_id: None,
            ed25519_public_key_pem: None,
            ed25519_private_key_pem: None,
            also_known_as: vec![],
            moved_to_ap_id: None,
            is_local: true,
            actor_type: "Person".into(),
        },
    )
    .await
    .unwrap();

    let (inbox_url, capture) = spawn_capture_server().await;
    let state = AppState::from_pool(pool.clone(), make_config());

    let activity = serde_json::json!({"type": "Create", "actor": local.ap_id});
    let queued =
        sakurasato_server::delivery::enqueue_activity(&pool, local.id, &inbox_url, &activity)
            .await
            .unwrap();

    let (tx, rx) = watch::channel(false);
    let handle = sakurasato_server::delivery::worker::spawn(state.clone(), rx);

    // worker は IDLE_TICK = 5s で polling するが、初回は即拾うので
    // 数百 ms 以内で配送が終わる想定。余裕を見て 3 秒待つ。
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let row = sakurasato_core::repo::delivery_queue::get_by_id(&pool, queued.id)
            .await
            .unwrap()
            .unwrap();
        if row.state == "delivered" {
            break;
        }
        assert!(
            std::time::Instant::now() <= deadline,
            "worker did not drain queue row in 3s; state = {}",
            row.state,
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // 受信側にも届いていること。
    let captured = capture
        .lock()
        .unwrap()
        .clone()
        .expect("capture server received no request");
    assert!(!captured.body.is_empty());

    // shutdown を送って worker を停止。await で抜けるはず。
    let _ = tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
}
