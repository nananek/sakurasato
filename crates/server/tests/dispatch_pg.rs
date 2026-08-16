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
            public_listen: None,
            local_api_listen: None,
            user: LOCAL_USER.into(),
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
        manually_approves_followers: false,
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
        manually_approves_followers: false,
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

/// Mastodon は `Accept.object` を **ネストされた Follow オブジェクト** で
/// 返す (URI 文字列ではなく)。その Follow 内側の `actor` は **我々**
/// (= sakurasato/me)、署名者は Mastodon 側の Bob、で必然的に不一致。
/// `dispatch` の最上段で `verify_nested_object_actor` を unconditional に
/// 走らせていた既存挙動だと、この Mastodon 由来 Accept が全部 401 で
/// 弾かれて follow が pending のまま残る (= 連合途絶) のが
/// `tests/federation/test_mastodon.py::TestNoteFromMastodon` で踏んだ
/// 既知不具合。**Create/Update/Delete に限定して呼ぶ** ことで Accept は
/// 通過するようになる。本テストはその回帰を 1 行で守るためのもの。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn accept_with_nested_follow_object_marks_follow_accepted(pool: PgPool) {
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

    let follow_ap_id = format!("https://{LOCAL_HOST}/users/{LOCAL_USER}/activities/follow-test");
    let follow = repo::follow::insert_pending(&pool, &follow_ap_id, local.id, remote.id)
        .await
        .unwrap();

    let state = AppState::from_pool(pool.clone(), make_config());
    let app = router(state);

    // Mastodon 形式: object はネストされた Follow object。`Follow.actor` は
    // **local** (= 我々) で signer (= remote) と一致しない。
    let body = serde_json::json!({
        "id": "https://remote.test/users/bob/activities/accept-1",
        "type": "Accept",
        "actor": remote.ap_id,
        "object": {
            "id": follow_ap_id.clone(),
            "type": "Follow",
            "actor": local.ap_id.clone(),
            "object": remote.ap_id,
        },
    })
    .to_string();
    let keyid = format!("{}#main-key", remote.ap_id);
    let req = build_signed_post(body.as_bytes(), "/inbox", &remote_priv, &keyid, LOCAL_HOST);

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "Mastodon-style nested Accept must not be blocked by nested-actor check"
    );

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
            manually_approves_followers: false,
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

// ============================================================================
// M9: Move 受領テスト
// ============================================================================

fn remote_actor_with_aka(
    host: &str,
    user: &str,
    pub_pem: &str,
    inbox_url: &str,
    also_known_as: Vec<String>,
) -> NewActor {
    let mut a = remote_actor(host, user, pub_pem, inbox_url);
    a.also_known_as = also_known_as;
    a
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn move_marks_source_moved_and_queues_auto_follow(pool: PgPool) {
    // 流れ:
    // 1. local が old@remote1 を follow している (accepted) 状態を用意。
    // 2. new@remote2 を pre-insert (alsoKnownAs に old を載せておく) →
    //    Move handler は HTTP fetch せずに DB から target を取れる。
    // 3. old@remote1 が Move を投げる → local の auto-follow が new@remote2 に
    //    対して queue されること、old の moved_to_ap_id が立つことを確認。
    let (_lp, local_pub) = fresh_rsa();
    let (old_priv, old_pub) = fresh_rsa();
    let (_np, new_pub) = fresh_rsa();

    let local = repo::actor::insert(&pool, local_actor(&local_pub, "PRIV"))
        .await
        .unwrap();
    let old = repo::actor::insert(
        &pool,
        remote_actor(
            "old.test",
            "alice",
            &old_pub,
            "https://old.test/users/alice/inbox",
        ),
    )
    .await
    .unwrap();
    let new = repo::actor::insert(
        &pool,
        remote_actor_with_aka(
            "new.test",
            "alice",
            &new_pub,
            "https://new.test/users/alice/inbox",
            vec![old.ap_id.clone()],
        ),
    )
    .await
    .unwrap();

    // local が old を follow している (accepted) 既存状態。
    let prev_follow_ap_id = format!(
        "https://{LOCAL_HOST}/users/{LOCAL_USER}/activities/follow-old-{}",
        old.id
    );
    let prev = repo::follow::insert_pending(&pool, &prev_follow_ap_id, local.id, old.id)
        .await
        .unwrap();
    repo::follow::set_state(
        &pool,
        prev.id,
        sakurasato_core::model::FollowState::Accepted,
    )
    .await
    .unwrap();

    let state = AppState::from_pool(pool.clone(), make_config());
    let app = router(state);

    let move_id = format!("https://old.test/users/alice/activities/move-{}", old.id);
    let body = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": move_id,
        "type": "Move",
        "actor": old.ap_id,
        "object": old.ap_id,
        "target": new.ap_id,
    })
    .to_string();
    let keyid = format!("{}#main-key", old.ap_id);
    let req = build_signed_post(body.as_bytes(), "/inbox", &old_priv, &keyid, LOCAL_HOST);

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED, "Move must be accepted");

    // 移動元 actor の moved_to_ap_id が立っている。
    let refreshed_old = repo::actor::get_by_id(&pool, old.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        refreshed_old.moved_to_ap_id.as_deref(),
        Some(new.ap_id.as_str())
    );

    // local 側に new への pending follow が作られている。
    let new_follows = sqlx::query!(
        "SELECT id, state FROM follow WHERE follower_actor_id = $1 AND followed_actor_id = $2",
        local.id,
        new.id,
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        new_follows.len(),
        1,
        "auto-follow row to new actor must exist"
    );
    assert_eq!(new_follows[0].state, "pending");

    // delivery_queue に Follow が積まれている (new actor inbox 宛)。
    let queued = sqlx::query!(
        r#"SELECT inbox_url, activity as "activity: sqlx::types::Json<serde_json::Value>"
           FROM delivery_queue WHERE sender_actor_id = $1 AND state = 'pending'"#,
        local.id,
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(queued.len(), 1, "auto-Follow must be queued");
    assert_eq!(queued[0].inbox_url, new.inbox_url);
    let activity = &queued[0].activity.0;
    assert_eq!(activity["type"], "Follow");
    assert_eq!(activity["actor"], local.ap_id);
    assert_eq!(activity["object"], new.ap_id);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn move_without_target_aka_consent_is_rejected(pool: PgPool) {
    // 双方向同意検査: target.alsoKnownAs に source が居ないと拒否すること。
    let (_lp, local_pub) = fresh_rsa();
    let (old_priv, old_pub) = fresh_rsa();
    let (_np, new_pub) = fresh_rsa();

    let local = repo::actor::insert(&pool, local_actor(&local_pub, "PRIV"))
        .await
        .unwrap();
    let old = repo::actor::insert(
        &pool,
        remote_actor(
            "old.test",
            "alice",
            &old_pub,
            "https://old.test/users/alice/inbox",
        ),
    )
    .await
    .unwrap();
    // new actor の alsoKnownAs を空にしておく → 同意なし。
    let new = repo::actor::insert(
        &pool,
        remote_actor_with_aka(
            "new.test",
            "alice",
            &new_pub,
            "https://new.test/users/alice/inbox",
            vec![],
        ),
    )
    .await
    .unwrap();

    let state = AppState::from_pool(pool.clone(), make_config());
    let app = router(state);

    let body = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://old.test/users/alice/activities/move-evil",
        "type": "Move",
        "actor": old.ap_id,
        "object": old.ap_id,
        "target": new.ap_id,
    })
    .to_string();
    let keyid = format!("{}#main-key", old.ap_id);
    let req = build_signed_post(body.as_bytes(), "/inbox", &old_priv, &keyid, LOCAL_HOST);

    let resp = app.oneshot(req).await.unwrap();
    // [[m9-pr1-review]] round-2 [1]: 永続的な同意失敗は **4xx** で返す。
    // 503 を返すと Mastodon が無限にリトライしてくる。
    assert!(
        resp.status().is_client_error(),
        "Move without target.alsoKnownAs consent must be rejected with 4xx (not 5xx), got {}",
        resp.status(),
    );

    // source.moved_to_ap_id は変わっていない。
    let unchanged = repo::actor::get_by_id(&pool, old.id)
        .await
        .unwrap()
        .unwrap();
    assert!(unchanged.moved_to_ap_id.is_none());
    // local 側でも target への auto-Follow は積まれていない (= 拒否されたので
    // フォロー関係に派生変更が起きないことを明示)。
    let new_follow_rows = sqlx::query!(
        "SELECT count(*) as c FROM follow WHERE follower_actor_id = $1 AND followed_actor_id = $2",
        local.id,
        new.id,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(new_follow_rows.c.unwrap_or(0), 0);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn move_object_must_equal_signer(pool: PgPool) {
    // signer が他人の `Move` を装って投げてきても拒否 (object != signer.ap_id)。
    // F3 で body.actor == signer は確認済みだが、object も signer 本人を
    // 指す必要がある (= 「自分が動いた」と宣言できるのは本人だけ)。
    let (_lp, local_pub) = fresh_rsa();
    let (signer_priv, signer_pub) = fresh_rsa();

    let _local = repo::actor::insert(&pool, local_actor(&local_pub, "PRIV"))
        .await
        .unwrap();
    let signer_actor = repo::actor::insert(
        &pool,
        remote_actor(
            "remote.test",
            "bob",
            &signer_pub,
            "https://remote.test/users/bob/inbox",
        ),
    )
    .await
    .unwrap();

    let state = AppState::from_pool(pool.clone(), make_config());
    let app = router(state);

    let body = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://remote.test/users/bob/activities/move-spoof",
        "type": "Move",
        "actor": signer_actor.ap_id,
        // 他人 (carol) を動かしたことにする → 拒否されるべき。
        "object": "https://other.test/users/carol",
        "target": "https://new.test/users/carol",
    })
    .to_string();
    let keyid = format!("{}#main-key", signer_actor.ap_id);
    let req = build_signed_post(body.as_bytes(), "/inbox", &signer_priv, &keyid, LOCAL_HOST);

    let resp = app.oneshot(req).await.unwrap();
    // [[m9-pr1-review]] round-2 [1]: 永続的なフィールド不正は **4xx** で返す。
    assert!(
        resp.status().is_client_error(),
        "Move with object != signer must be rejected with 4xx, got {}",
        resp.status(),
    );
}
// =============================================================================
// M11 — Create / Delete / Update / Announce 受信テスト
// =============================================================================

/// followee からの Create を受信して `note` 行が立つこと。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn create_from_followee_inserts_remote_note(pool: PgPool) {
    let (_lp, local_pub) = fresh_rsa();
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

    // local が remote を accepted で follow している状態を用意。
    let follow_ap = format!("https://{LOCAL_HOST}/users/{LOCAL_USER}/activities/follow-bob");
    let f = repo::follow::insert_pending(&pool, &follow_ap, local.id, remote.id)
        .await
        .unwrap();
    repo::follow::set_state(&pool, f.id, sakurasato_core::model::FollowState::Accepted)
        .await
        .unwrap();

    let state = AppState::from_pool(pool.clone(), make_config());
    let app = router(state);

    let note_id = "https://remote.test/notes/note-1";
    let body = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://remote.test/users/bob/activities/create-1",
        "type": "Create",
        "actor": remote.ap_id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": note_id,
            "type": "Note",
            "attributedTo": remote.ap_id,
            "content": "hello sakurasato",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "published": "2026-05-31T12:00:00Z",
        },
    })
    .to_string();
    let keyid = format!("{}#main-key", remote.ap_id);
    let req = build_signed_post(body.as_bytes(), "/inbox", &remote_priv, &keyid, LOCAL_HOST);

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let stored = repo::note::get_by_ap_id(&pool, note_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.content, "hello sakurasato");
    assert_eq!(stored.actor_id, remote.id);
    assert!(!stored.is_local);
    assert_eq!(stored.visibility, "public");
}

/// followee でも mention でもない Create は **DB に入れない** (= filter)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn create_from_unrelated_actor_is_filtered_out(pool: PgPool) {
    let (_lp, local_pub) = fresh_rsa();
    let (remote_priv, remote_pub) = fresh_rsa();

    let _ = repo::actor::insert(&pool, local_actor(&local_pub, "PRIV"))
        .await
        .unwrap();
    let remote = repo::actor::insert(
        &pool,
        remote_actor(
            "remote.test",
            "carol",
            &remote_pub,
            "https://remote.test/users/carol/inbox",
        ),
    )
    .await
    .unwrap();

    let state = AppState::from_pool(pool.clone(), make_config());
    let app = router(state);

    let note_id = "https://remote.test/notes/note-99";
    let body = serde_json::json!({
        "id": "https://remote.test/users/carol/activities/create-99",
        "type": "Create",
        "actor": remote.ap_id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": note_id,
            "type": "Note",
            "attributedTo": remote.ap_id,
            "content": "stranger",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "published": "2026-05-31T12:00:00Z",
        },
    })
    .to_string();
    let keyid = format!("{}#main-key", remote.ap_id);
    let req = build_signed_post(body.as_bytes(), "/inbox", &remote_priv, &keyid, LOCAL_HOST);

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED, "filter は silent 202");

    let stored = repo::note::get_by_ap_id(&pool, note_id).await.unwrap();
    assert!(stored.is_none(), "filter された Note は DB に残らない");
}

/// 我々の actor が `to` / `cc` に居れば follow 関係が無くても取り込む (mention)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn create_addressed_to_us_is_stored_even_without_follow(pool: PgPool) {
    let (_lp, local_pub) = fresh_rsa();
    let (remote_priv, remote_pub) = fresh_rsa();

    let local = repo::actor::insert(&pool, local_actor(&local_pub, "PRIV"))
        .await
        .unwrap();
    let remote = repo::actor::insert(
        &pool,
        remote_actor(
            "remote.test",
            "dave",
            &remote_pub,
            "https://remote.test/users/dave/inbox",
        ),
    )
    .await
    .unwrap();

    let state = AppState::from_pool(pool.clone(), make_config());
    let app = router(state);

    let note_id = "https://remote.test/notes/mention-1";
    let body = serde_json::json!({
        "id": "https://remote.test/users/dave/activities/create-mention",
        "type": "Create",
        "actor": remote.ap_id,
        "to": [local.ap_id],
        "object": {
            "id": note_id,
            "type": "Note",
            "attributedTo": remote.ap_id,
            "content": "@alice hello",
            "to": [local.ap_id],
            "published": "2026-05-31T12:00:00Z",
        },
    })
    .to_string();
    let keyid = format!("{}#main-key", remote.ap_id);
    let req = build_signed_post(body.as_bytes(), "/inbox", &remote_priv, &keyid, LOCAL_HOST);

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let stored = repo::note::get_by_ap_id(&pool, note_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.actor_id, remote.id);
}

/// 同じ Note の Create を二度受け取っても 202 を返しつつ重複行は作らない。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn create_duplicate_is_idempotent(pool: PgPool) {
    let (_lp, local_pub) = fresh_rsa();
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
    let follow_ap = format!("https://{LOCAL_HOST}/users/{LOCAL_USER}/activities/follow-bob-dup");
    let f = repo::follow::insert_pending(&pool, &follow_ap, local.id, remote.id)
        .await
        .unwrap();
    repo::follow::set_state(&pool, f.id, sakurasato_core::model::FollowState::Accepted)
        .await
        .unwrap();

    let state = AppState::from_pool(pool.clone(), make_config());
    let app1 = router(state.clone());
    let app2 = router(state);

    let note_id = "https://remote.test/notes/dup-1";
    let body = serde_json::json!({
        "id": "https://remote.test/users/bob/activities/create-dup",
        "type": "Create",
        "actor": remote.ap_id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": note_id,
            "type": "Note",
            "attributedTo": remote.ap_id,
            "content": "dup",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "published": "2026-05-31T12:00:00Z",
        },
    })
    .to_string();
    let keyid = format!("{}#main-key", remote.ap_id);

    for app in [app1, app2] {
        let req = build_signed_post(body.as_bytes(), "/inbox", &remote_priv, &keyid, LOCAL_HOST);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
    }

    let count = sqlx::query!("SELECT count(*) as c FROM note WHERE ap_id = $1", note_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count.c.unwrap_or(0), 1, "second receipt must not duplicate");
}

/// 自分の Note の Delete を受け取ったら DB から消える。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn delete_by_author_removes_note(pool: PgPool) {
    let (_lp, local_pub) = fresh_rsa();
    let (remote_priv, remote_pub) = fresh_rsa();

    let _ = repo::actor::insert(&pool, local_actor(&local_pub, "PRIV"))
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

    let note_ap_id = "https://remote.test/notes/to-delete";
    let _note = repo::note::insert(
        &pool,
        sakurasato_core::repo::note::NewNote {
            ap_id: note_ap_id.into(),
            actor_id: remote.id,
            content: "bye".into(),
            language: None,
            in_reply_to_ap_id: None,
            in_reply_to_note_id: None,
            summary: None,
            visibility: sakurasato_core::model::Visibility::Public,
            sensitive: false,
            to_recipients: vec![],
            cc_recipients: vec![],
            attachments: serde_json::Value::Array(vec![]),
            tags: serde_json::Value::Array(vec![]),
            is_local: false,
            url: None,
            source: None,
            published_at: chrono::Utc::now(),
        },
    )
    .await
    .unwrap();

    let state = AppState::from_pool(pool.clone(), make_config());
    let app = router(state);

    let body = serde_json::json!({
        "id": "https://remote.test/users/bob/activities/delete-1",
        "type": "Delete",
        "actor": remote.ap_id,
        "object": {"type": "Tombstone", "id": note_ap_id},
    })
    .to_string();
    let keyid = format!("{}#main-key", remote.ap_id);
    let req = build_signed_post(body.as_bytes(), "/inbox", &remote_priv, &keyid, LOCAL_HOST);

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let after = repo::note::get_by_ap_id(&pool, note_ap_id).await.unwrap();
    assert!(after.is_none(), "Note must be deleted by author");
}

/// 別の actor が他人の Note を Delete しようとしたら 400 (Malformed)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn delete_by_non_author_is_rejected(pool: PgPool) {
    let (_lp, local_pub) = fresh_rsa();
    let (_op, owner_pub) = fresh_rsa();
    let (attacker_priv, attacker_pub) = fresh_rsa();

    let _ = repo::actor::insert(&pool, local_actor(&local_pub, "PRIV"))
        .await
        .unwrap();
    let owner = repo::actor::insert(
        &pool,
        remote_actor(
            "remote.test",
            "owner",
            &owner_pub,
            "https://remote.test/users/owner/inbox",
        ),
    )
    .await
    .unwrap();
    let attacker = repo::actor::insert(
        &pool,
        remote_actor(
            "evil.test",
            "eve",
            &attacker_pub,
            "https://evil.test/users/eve/inbox",
        ),
    )
    .await
    .unwrap();
    let _ = attacker;

    let note_ap_id = "https://remote.test/notes/owners-note";
    repo::note::insert(
        &pool,
        sakurasato_core::repo::note::NewNote {
            ap_id: note_ap_id.into(),
            actor_id: owner.id,
            content: "mine".into(),
            language: None,
            in_reply_to_ap_id: None,
            in_reply_to_note_id: None,
            summary: None,
            visibility: sakurasato_core::model::Visibility::Public,
            sensitive: false,
            to_recipients: vec![],
            cc_recipients: vec![],
            attachments: serde_json::Value::Array(vec![]),
            tags: serde_json::Value::Array(vec![]),
            is_local: false,
            url: None,
            source: None,
            published_at: chrono::Utc::now(),
        },
    )
    .await
    .unwrap();

    let state = AppState::from_pool(pool.clone(), make_config());
    let app = router(state);

    let body = serde_json::json!({
        "id": "https://evil.test/users/eve/activities/delete-spoof",
        "type": "Delete",
        "actor": "https://evil.test/users/eve",
        "object": note_ap_id,
    })
    .to_string();
    let keyid = "https://evil.test/users/eve#main-key";
    let req = build_signed_post(body.as_bytes(), "/inbox", &attacker_priv, keyid, LOCAL_HOST);

    let resp = app.oneshot(req).await.unwrap();
    assert!(
        resp.status().is_client_error(),
        "Delete by non-author must be 4xx, got {}",
        resp.status(),
    );

    let still = repo::note::get_by_ap_id(&pool, note_ap_id).await.unwrap();
    assert!(still.is_some(), "Note must not be deleted by attacker");
}

/// 未知 Note の Delete は silent 202 (= 我々が持っていない note を消せと
/// 言われても落ちない)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn delete_of_unknown_note_is_silent(pool: PgPool) {
    let (_lp, local_pub) = fresh_rsa();
    let (remote_priv, remote_pub) = fresh_rsa();

    let _ = repo::actor::insert(&pool, local_actor(&local_pub, "PRIV"))
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

    let body = serde_json::json!({
        "id": "https://remote.test/users/bob/activities/delete-unknown",
        "type": "Delete",
        "actor": remote.ap_id,
        "object": "https://remote.test/notes/never-seen",
    })
    .to_string();
    let keyid = format!("{}#main-key", remote.ap_id);
    let req = build_signed_post(body.as_bytes(), "/inbox", &remote_priv, &keyid, LOCAL_HOST);

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
}

/// 自分の Note の Update で content と `edited_at` が動く。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn update_note_by_author_changes_content(pool: PgPool) {
    let (_lp, local_pub) = fresh_rsa();
    let (remote_priv, remote_pub) = fresh_rsa();

    let _ = repo::actor::insert(&pool, local_actor(&local_pub, "PRIV"))
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

    let note_ap_id = "https://remote.test/notes/edit-target";
    let original = repo::note::insert(
        &pool,
        sakurasato_core::repo::note::NewNote {
            ap_id: note_ap_id.into(),
            actor_id: remote.id,
            content: "original".into(),
            language: None,
            in_reply_to_ap_id: None,
            in_reply_to_note_id: None,
            summary: None,
            visibility: sakurasato_core::model::Visibility::Public,
            sensitive: false,
            to_recipients: vec![],
            cc_recipients: vec![],
            attachments: serde_json::Value::Array(vec![]),
            tags: serde_json::Value::Array(vec![]),
            is_local: false,
            url: None,
            source: None,
            published_at: chrono::Utc::now(),
        },
    )
    .await
    .unwrap();
    assert!(original.edited_at.is_none());

    let state = AppState::from_pool(pool.clone(), make_config());
    let app = router(state);

    let body = serde_json::json!({
        "id": "https://remote.test/users/bob/activities/update-1",
        "type": "Update",
        "actor": remote.ap_id,
        "object": {
            "id": note_ap_id,
            "type": "Note",
            "attributedTo": remote.ap_id,
            "content": "edited!",
            "updated": "2026-06-01T00:00:00Z",
        },
    })
    .to_string();
    let keyid = format!("{}#main-key", remote.ap_id);
    let req = build_signed_post(body.as_bytes(), "/inbox", &remote_priv, &keyid, LOCAL_HOST);

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let after = repo::note::get_by_ap_id(&pool, note_ap_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.content, "edited!");
    assert!(after.edited_at.is_some(), "edited_at must be set");
}

/// 別 actor が他人の Note を Update しようとしたら 4xx (Malformed)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn update_note_by_non_author_is_rejected(pool: PgPool) {
    let (_lp, local_pub) = fresh_rsa();
    let (_op, owner_pub) = fresh_rsa();
    let (attacker_priv, attacker_pub) = fresh_rsa();

    let _ = repo::actor::insert(&pool, local_actor(&local_pub, "PRIV"))
        .await
        .unwrap();
    let owner = repo::actor::insert(
        &pool,
        remote_actor(
            "remote.test",
            "owner",
            &owner_pub,
            "https://remote.test/users/owner/inbox",
        ),
    )
    .await
    .unwrap();
    let _ = repo::actor::insert(
        &pool,
        remote_actor(
            "evil.test",
            "eve",
            &attacker_pub,
            "https://evil.test/users/eve/inbox",
        ),
    )
    .await
    .unwrap();

    let note_ap_id = "https://remote.test/notes/owners";
    repo::note::insert(
        &pool,
        sakurasato_core::repo::note::NewNote {
            ap_id: note_ap_id.into(),
            actor_id: owner.id,
            content: "untouched".into(),
            language: None,
            in_reply_to_ap_id: None,
            in_reply_to_note_id: None,
            summary: None,
            visibility: sakurasato_core::model::Visibility::Public,
            sensitive: false,
            to_recipients: vec![],
            cc_recipients: vec![],
            attachments: serde_json::Value::Array(vec![]),
            tags: serde_json::Value::Array(vec![]),
            is_local: false,
            url: None,
            source: None,
            published_at: chrono::Utc::now(),
        },
    )
    .await
    .unwrap();

    let state = AppState::from_pool(pool.clone(), make_config());
    let app = router(state);

    // F3 をすり抜けるため、attacker は own attributedTo で送る必要がある。
    // すると F3 nested check で attributedTo == signer は通るが、handler の
    // note.actor_id 検査で落ちる。Note: attributedTo を本人にしつつ object.id
    // は他人 note の URI ── これは "全く知らない note を eve が更新したい"
    // ケースとみなされ、handler は note.actor_id (= owner.id) != signer.id
    // で 400 を返す。
    let body = serde_json::json!({
        "id": "https://evil.test/users/eve/activities/update-spoof",
        "type": "Update",
        "actor": "https://evil.test/users/eve",
        "object": {
            "id": note_ap_id,
            "type": "Note",
            "attributedTo": "https://evil.test/users/eve",
            "content": "hijacked",
        },
    })
    .to_string();
    let keyid = "https://evil.test/users/eve#main-key";
    let req = build_signed_post(body.as_bytes(), "/inbox", &attacker_priv, keyid, LOCAL_HOST);

    let resp = app.oneshot(req).await.unwrap();
    assert!(
        resp.status().is_client_error(),
        "Update for someone else's note must be 4xx, got {}",
        resp.status(),
    );

    let still = repo::note::get_by_ap_id(&pool, note_ap_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(still.content, "untouched");
}

/// Update Actor で object.id が signer `ap_id` と一致しなければ拒否 (fetch 前に弾く)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn update_actor_with_mismatched_object_id_is_rejected(pool: PgPool) {
    let (_lp, local_pub) = fresh_rsa();
    let (remote_priv, remote_pub) = fresh_rsa();

    let _ = repo::actor::insert(&pool, local_actor(&local_pub, "PRIV"))
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

    let body = serde_json::json!({
        "id": "https://remote.test/users/bob/activities/update-actor-spoof",
        "type": "Update",
        "actor": remote.ap_id,
        "object": {
            "id": "https://other.test/users/carol",
            "type": "Person",
        },
    })
    .to_string();
    let keyid = format!("{}#main-key", remote.ap_id);
    let req = build_signed_post(body.as_bytes(), "/inbox", &remote_priv, &keyid, LOCAL_HOST);

    let resp = app.oneshot(req).await.unwrap();
    assert!(
        resp.status().is_client_error(),
        "Update Actor with mismatched object.id must be 4xx, got {}",
        resp.status(),
    );
}

/// followee からの Announce で `announce` 行が作られる。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn announce_from_followee_inserts_row(pool: PgPool) {
    let (_lp, local_pub) = fresh_rsa();
    let (remote_priv, remote_pub) = fresh_rsa();
    let (_ap, author_pub) = fresh_rsa();

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
    let author = repo::actor::insert(
        &pool,
        remote_actor(
            "other.test",
            "alice",
            &author_pub,
            "https://other.test/users/alice/inbox",
        ),
    )
    .await
    .unwrap();
    let follow_ap = format!("https://{LOCAL_HOST}/users/{LOCAL_USER}/activities/follow-bob-ann");
    let f = repo::follow::insert_pending(&pool, &follow_ap, local.id, remote.id)
        .await
        .unwrap();
    repo::follow::set_state(&pool, f.id, sakurasato_core::model::FollowState::Accepted)
        .await
        .unwrap();

    let note_ap_id = "https://other.test/notes/known-1";
    let note = repo::note::insert(
        &pool,
        sakurasato_core::repo::note::NewNote {
            ap_id: note_ap_id.into(),
            actor_id: author.id,
            content: "boost me".into(),
            language: None,
            in_reply_to_ap_id: None,
            in_reply_to_note_id: None,
            summary: None,
            visibility: sakurasato_core::model::Visibility::Public,
            sensitive: false,
            to_recipients: vec![],
            cc_recipients: vec![],
            attachments: serde_json::Value::Array(vec![]),
            tags: serde_json::Value::Array(vec![]),
            is_local: false,
            url: None,
            source: None,
            published_at: chrono::Utc::now(),
        },
    )
    .await
    .unwrap();

    let state = AppState::from_pool(pool.clone(), make_config());
    let app = router(state);

    let announce_ap = "https://remote.test/users/bob/activities/announce-1";
    let body = serde_json::json!({
        "id": announce_ap,
        "type": "Announce",
        "actor": remote.ap_id,
        "object": note_ap_id,
        "published": "2026-05-31T13:00:00Z",
    })
    .to_string();
    let keyid = format!("{}#main-key", remote.ap_id);
    let req = build_signed_post(body.as_bytes(), "/inbox", &remote_priv, &keyid, LOCAL_HOST);

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let row = sakurasato_core::repo::announce::get_by_ap_id(&pool, announce_ap)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.note_id, note.id);
    assert_eq!(row.actor_id, remote.id);

    // 報告バグの回帰防止: 第三者 (alice) の note を followee (bob) が boost した
    // だけでは「自分への通知」を作らない (home timeline の内容であって通知では
    // ないため)。announce 行は作るが notification 行は 0。
    let unread = sakurasato_core::repo::notification::count_unread(&pool, local.id)
        .await
        .unwrap();
    assert_eq!(
        unread, 0,
        "third-party note boost must not create a self-notification"
    );
}

/// followee が **自分 (local) の note** を boost したときは通知が作られる。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn announce_of_own_note_creates_notification(pool: PgPool) {
    let (_lp, local_pub) = fresh_rsa();
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
    let follow_ap = format!("https://{LOCAL_HOST}/users/{LOCAL_USER}/activities/follow-bob-own");
    let f = repo::follow::insert_pending(&pool, &follow_ap, local.id, remote.id)
        .await
        .unwrap();
    repo::follow::set_state(&pool, f.id, sakurasato_core::model::FollowState::Accepted)
        .await
        .unwrap();

    // 自分 (local) の note を作る (is_local = true)。
    let note_ap_id = format!("https://{LOCAL_HOST}/notes/own-1");
    let note = repo::note::insert(
        &pool,
        sakurasato_core::repo::note::NewNote {
            ap_id: note_ap_id.clone(),
            actor_id: local.id,
            content: "my own note".into(),
            language: None,
            in_reply_to_ap_id: None,
            in_reply_to_note_id: None,
            summary: None,
            visibility: sakurasato_core::model::Visibility::Public,
            sensitive: false,
            to_recipients: vec![],
            cc_recipients: vec![],
            attachments: serde_json::Value::Array(vec![]),
            tags: serde_json::Value::Array(vec![]),
            is_local: true,
            url: None,
            source: None,
            published_at: chrono::Utc::now(),
        },
    )
    .await
    .unwrap();

    let state = AppState::from_pool(pool.clone(), make_config());
    let app = router(state);

    let announce_ap = "https://remote.test/users/bob/activities/announce-own-1";
    let body = serde_json::json!({
        "id": announce_ap,
        "type": "Announce",
        "actor": remote.ap_id,
        "object": note_ap_id,
        "published": "2026-05-31T13:00:00Z",
    })
    .to_string();
    let keyid = format!("{}#main-key", remote.ap_id);
    let req = build_signed_post(body.as_bytes(), "/inbox", &remote_priv, &keyid, LOCAL_HOST);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // announce 行 + 通知 1 行 (自分の note なので)。
    let row = sakurasato_core::repo::announce::get_by_ap_id(&pool, announce_ap)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.note_id, note.id);
    let unread = sakurasato_core::repo::notification::count_unread(&pool, local.id)
        .await
        .unwrap();
    assert_eq!(
        unread, 1,
        "boost of our own note must create a notification"
    );
}

/// 未知 Note への Announce は fetch を試みるが (Issue #266)、fetch 不能なら
/// boost を捨てる (= 行を作らない)。ここでは `object` を loopback URL にして
/// SSRF ガードで **即時** fetch 失敗させる (ネットワーク I/O 無しで決定的)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn announce_of_unfetchable_unknown_note_drops_boost(pool: PgPool) {
    let (_lp, local_pub) = fresh_rsa();
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
    let follow_ap = format!("https://{LOCAL_HOST}/users/{LOCAL_USER}/activities/follow-bob-unk");
    let f = repo::follow::insert_pending(&pool, &follow_ap, local.id, remote.id)
        .await
        .unwrap();
    repo::follow::set_state(&pool, f.id, sakurasato_core::model::FollowState::Accepted)
        .await
        .unwrap();

    let state = AppState::from_pool(pool.clone(), make_config());
    let app = router(state);

    let announce_ap = "https://remote.test/users/bob/activities/announce-unknown";
    let body = serde_json::json!({
        "id": announce_ap,
        "type": "Announce",
        "actor": remote.ap_id,
        // loopback → net_guard が即 Blocked で弾く (= 外向き接続を試さない)。
        "object": "http://127.0.0.1/notes/never-seen",
        "published": "2026-05-31T13:00:00Z",
    })
    .to_string();
    let keyid = format!("{}#main-key", remote.ap_id);
    let req = build_signed_post(body.as_bytes(), "/inbox", &remote_priv, &keyid, LOCAL_HOST);

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let row = sakurasato_core::repo::announce::get_by_ap_id(&pool, announce_ap)
        .await
        .unwrap();
    assert!(
        row.is_none(),
        "fetch 不能な unknown Note の boost は記録しない",
    );
}

/// Undo Announce で `announce` 行が消える。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn undo_announce_removes_row(pool: PgPool) {
    let (_lp, local_pub) = fresh_rsa();
    let (remote_priv, remote_pub) = fresh_rsa();
    let (_ap, author_pub) = fresh_rsa();

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
    let author = repo::actor::insert(
        &pool,
        remote_actor(
            "other.test",
            "alice",
            &author_pub,
            "https://other.test/users/alice/inbox",
        ),
    )
    .await
    .unwrap();
    let follow_ap =
        format!("https://{LOCAL_HOST}/users/{LOCAL_USER}/activities/follow-bob-undoann");
    let f = repo::follow::insert_pending(&pool, &follow_ap, local.id, remote.id)
        .await
        .unwrap();
    repo::follow::set_state(&pool, f.id, sakurasato_core::model::FollowState::Accepted)
        .await
        .unwrap();
    let note_ap_id = "https://other.test/notes/undo-target";
    let note = repo::note::insert(
        &pool,
        sakurasato_core::repo::note::NewNote {
            ap_id: note_ap_id.into(),
            actor_id: author.id,
            content: "x".into(),
            language: None,
            in_reply_to_ap_id: None,
            in_reply_to_note_id: None,
            summary: None,
            visibility: sakurasato_core::model::Visibility::Public,
            sensitive: false,
            to_recipients: vec![],
            cc_recipients: vec![],
            attachments: serde_json::Value::Array(vec![]),
            tags: serde_json::Value::Array(vec![]),
            is_local: false,
            url: None,
            source: None,
            published_at: chrono::Utc::now(),
        },
    )
    .await
    .unwrap();
    let announce_ap = "https://remote.test/users/bob/activities/announce-undo";
    sakurasato_core::repo::announce::insert_or_get(
        &pool,
        announce_ap,
        note.id,
        remote.id,
        chrono::Utc::now(),
    )
    .await
    .unwrap();

    let state = AppState::from_pool(pool.clone(), make_config());
    let app = router(state);

    let body = serde_json::json!({
        "id": "https://remote.test/users/bob/activities/undo-1",
        "type": "Undo",
        "actor": remote.ap_id,
        "object": {
            "id": announce_ap,
            "type": "Announce",
            "actor": remote.ap_id,
            "object": note_ap_id,
        },
    })
    .to_string();
    let keyid = format!("{}#main-key", remote.ap_id);
    let req = build_signed_post(body.as_bytes(), "/inbox", &remote_priv, &keyid, LOCAL_HOST);

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let row = sakurasato_core::repo::announce::get_by_ap_id(&pool, announce_ap)
        .await
        .unwrap();
    assert!(row.is_none(), "Undo Announce must remove the row");
}

// ---------------------------------------------------------------------------
// Issue #66 (鍵アカ / manuallyApprovesFollowers) ── M12
// ---------------------------------------------------------------------------

fn locked_local_actor(pub_pem: &str, priv_pem: &str) -> NewActor {
    let mut a = local_actor(pub_pem, priv_pem);
    a.manually_approves_followers = true;
    a
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn locked_actor_keeps_inbound_follow_pending(pool: PgPool) {
    // 鍵アカ (`manually_approves_followers = TRUE`) の local actor 宛 Follow
    // は auto-Accept されず `follow.state = pending` で据え置かれ、
    // `delivery_queue` に Accept は積まれない (Issue #66)。
    let (local_priv, local_pub) = fresh_rsa();
    let (remote_priv, remote_pub) = fresh_rsa();
    let _ = local_priv;

    let local = repo::actor::insert(&pool, locked_local_actor(&local_pub, "PRIV"))
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

    let follow_id = format!(
        "https://remote.test/users/bob/activities/locked-follow-{}",
        local.id,
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
        "locked actor still returns 202 for the inbound Follow (silent hold)",
    );

    let row = sqlx::query!("SELECT state FROM follow WHERE ap_id = $1", follow_id,)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        row.state, "pending",
        "locked actor must keep follow row at pending until CLI approval",
    );

    let queued = sqlx::query!(
        "SELECT count(*) AS c FROM delivery_queue WHERE sender_actor_id = $1",
        local.id,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        queued.c.unwrap_or(0),
        0,
        "locked actor must NOT auto-enqueue an Accept activity",
    );

    let inboxes = repo::follow::list_accepted_inboxes(&pool, local.id)
        .await
        .unwrap();
    assert!(
        !inboxes.iter().any(|u| u == &remote_inbox),
        "follower of a still-pending Follow must not appear as a delivery target",
    );
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn locked_actor_existing_accepted_follow_still_re_accepts(pool: PgPool) {
    // unlock → followed → lock の順に状態が遷移した actor に対し、Mastodon
    // が retry で再送してきた既存 Follow が来たケース。`row.state =
    // accepted` のまま `Accept を再送出` するブランチに入る (state を
    // Pending に巻き戻さない / lock 後でも再 Accept は queue する)。
    let (local_priv, local_pub) = fresh_rsa();
    let (remote_priv, remote_pub) = fresh_rsa();
    let _ = local_priv;

    let local = repo::actor::insert(&pool, locked_local_actor(&local_pub, "PRIV"))
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

    // 既存 accepted の follow row を事前に積んでおく (= unlock 時代の名残)。
    let follow_id = format!(
        "https://remote.test/users/bob/activities/preexisting-{}",
        local.id
    );
    sqlx::query!(
        r"INSERT INTO follow (ap_id, follower_actor_id, followed_actor_id, state)
          VALUES ($1, $2, $3, 'accepted')",
        follow_id,
        remote.id,
        local.id,
    )
    .execute(&pool)
    .await
    .unwrap();

    let state = AppState::from_pool(pool.clone(), make_config());
    let app = router(state);
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
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // 既存 accepted → Accept 再送 ── state は accepted のまま据え置き、
    // delivery_queue に Accept が 1 行追加される。
    let row = sqlx::query!("SELECT state FROM follow WHERE ap_id = $1", follow_id,)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.state, "accepted");

    let queued = sqlx::query!(
        "SELECT count(*) AS c FROM delivery_queue WHERE sender_actor_id = $1",
        local.id,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        queued.c.unwrap_or(0),
        1,
        "Accept must be re-enqueued for the retry, even on locked actor",
    );
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn follow_request_list_returns_only_pending_local(pool: PgPool) {
    // 鍵アカ 1 + remote actor を作り、`follow` 行を 3 つ (pending/accepted/
    // pending-but-followed-is-remote) 仕込む。`list_pending_for_local` は
    // 1 件目だけ返す。
    let (_, local_pub) = fresh_rsa();
    let (_, remote_pub) = fresh_rsa();

    let local = repo::actor::insert(&pool, locked_local_actor(&local_pub, "PRIV"))
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
    let (_, remote2_pub) = fresh_rsa();
    let remote2 = repo::actor::insert(
        &pool,
        remote_actor(
            "remote.test",
            "carol",
            &remote2_pub,
            "https://remote.test/users/carol/inbox",
        ),
    )
    .await
    .unwrap();

    // 1) bob → local pending → 列挙される
    let visible_ap_id = "https://remote.test/users/bob/activities/visible".to_string();
    sqlx::query!(
        r"INSERT INTO follow (ap_id, follower_actor_id, followed_actor_id, state)
          VALUES ($1, $2, $3, 'pending')",
        visible_ap_id,
        remote.id,
        local.id,
    )
    .execute(&pool)
    .await
    .unwrap();

    // 2) carol → local accepted → 列挙されない (state フィルタ)
    sqlx::query!(
        r"INSERT INTO follow (ap_id, follower_actor_id, followed_actor_id, state)
          VALUES ($1, $2, $3, 'accepted')",
        "https://remote.test/users/carol/activities/already-accepted",
        remote2.id,
        local.id,
    )
    .execute(&pool)
    .await
    .unwrap();

    // 3) local → carol pending (= outbound follow 待ち) → 列挙されない
    //    (followed が remote actor なので `followed.is_local = TRUE` フィルタで落ちる)
    sqlx::query!(
        r"INSERT INTO follow (ap_id, follower_actor_id, followed_actor_id, state)
          VALUES ($1, $2, $3, 'pending')",
        "https://sakura.test/users/alice/activities/outbound",
        local.id,
        remote2.id,
    )
    .execute(&pool)
    .await
    .unwrap();

    let pending = repo::follow::list_pending_for_local(&pool).await.unwrap();
    assert_eq!(
        pending.len(),
        1,
        "only the inbound pending row must show up"
    );
    let (_, ap_id, follower_ap, _) = &pending[0];
    assert_eq!(ap_id, &visible_ap_id);
    assert_eq!(follower_ap, &remote.ap_id);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn follow_request_approve_enqueues_accept_and_flips_state(pool: PgPool) {
    use sakurasato_core::model::FollowState;
    use sakurasato_server::follow_request;

    let (_, local_pub) = fresh_rsa();
    let (_, remote_pub) = fresh_rsa();

    let local = repo::actor::insert(&pool, locked_local_actor(&local_pub, "PRIV"))
        .await
        .unwrap();
    let remote_inbox = "https://remote.test/users/bob/inbox".to_string();
    let remote = repo::actor::insert(
        &pool,
        remote_actor("remote.test", "bob", &remote_pub, &remote_inbox),
    )
    .await
    .unwrap();

    let follow_ap_id = "https://remote.test/users/bob/activities/pending-1".to_string();
    let follow_id: i64 = sqlx::query_scalar!(
        r"INSERT INTO follow (ap_id, follower_actor_id, followed_actor_id, state)
          VALUES ($1, $2, $3, 'pending') RETURNING id",
        follow_ap_id,
        remote.id,
        local.id,
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    let state = AppState::from_pool(pool.clone(), make_config());
    follow_request::approve_or_reject(&state, follow_id, FollowState::Accepted)
        .await
        .unwrap();

    let row = sqlx::query!("SELECT state FROM follow WHERE id = $1", follow_id,)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.state, "accepted");

    let queue = sqlx::query!(
        r#"SELECT inbox_url, activity as "activity: sqlx::types::Json<serde_json::Value>"
           FROM delivery_queue WHERE sender_actor_id = $1"#,
        local.id,
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(queue.len(), 1, "Accept queued exactly once");
    let q = &queue[0];
    assert_eq!(q.inbox_url, remote_inbox);
    assert_eq!(q.activity.0["type"], "Accept");
    assert_eq!(q.activity.0["actor"], local.ap_id);
    // object には最小 Follow JSON (id/type/actor/object) が inline で埋まる。
    assert_eq!(q.activity.0["object"]["id"], follow_ap_id);
    assert_eq!(q.activity.0["object"]["actor"], remote.ap_id);
    assert_eq!(q.activity.0["object"]["object"], local.ap_id);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn unlock_does_not_auto_accept_pending_follows(pool: PgPool) {
    // **PR #80 round-2 review #10 (invariant test)**: lock 中に届いた Follow が
    // pending で据え置かれている状態で actor を unlock したとき、pending 行が
    // **勝手に accepted に倒れない** ことを保証する。設計意図 (= 鍵 ON 中の
    // 「待ち」を unlock の事故で全部 accept してしまわない) を将来のリファクタで
    // 意図せず壊さないための guard。
    use sakurasato_server::actor_admin;

    let (_, local_pub) = fresh_rsa();
    let (_, remote_pub) = fresh_rsa();

    let local = repo::actor::insert(&pool, locked_local_actor(&local_pub, "PRIV"))
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

    let pending_ap_id = "https://remote.test/users/bob/activities/locked-pending".to_string();
    sqlx::query!(
        r"INSERT INTO follow (ap_id, follower_actor_id, followed_actor_id, state)
          VALUES ($1, $2, $3, 'pending')",
        pending_ap_id,
        remote.id,
        local.id,
    )
    .execute(&pool)
    .await
    .unwrap();

    // unlock 経路を直叩き (= CLI / local API どちらでも同じヘルパに集約)。
    let state = AppState::from_pool(pool.clone(), make_config());
    let outcome = actor_admin::set_lock_state(&state, false).await.unwrap();
    assert!(outcome.changed, "lock state must have flipped");
    assert!(
        !outcome.updated.manually_approves_followers,
        "actor must be unlocked",
    );

    // **不変式**: pending 行は依然 pending のまま、accepted には倒れない。
    let after = sqlx::query!("SELECT state FROM follow WHERE ap_id = $1", pending_ap_id,)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        after.state, "pending",
        "unlock must NOT auto-accept the previously-pending follow row",
    );

    // Accept activity が queue されていないことも確認 ── 自動承認の副作用が
    // delivery_queue 経由で起きないこと。
    let queued = sqlx::query!(
        "SELECT count(*) AS c FROM delivery_queue \
         WHERE sender_actor_id = $1 \
           AND activity->>'type' = 'Accept'",
        local.id,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        queued.c.unwrap_or(0),
        0,
        "no Accept must be queued by an unlock operation",
    );
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn follow_request_reject_enqueues_reject_and_flips_state(pool: PgPool) {
    use sakurasato_core::model::FollowState;
    use sakurasato_server::follow_request;

    let (_, local_pub) = fresh_rsa();
    let (_, remote_pub) = fresh_rsa();

    let local = repo::actor::insert(&pool, locked_local_actor(&local_pub, "PRIV"))
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

    let follow_ap_id = "https://remote.test/users/bob/activities/pending-rej".to_string();
    let follow_id: i64 = sqlx::query_scalar!(
        r"INSERT INTO follow (ap_id, follower_actor_id, followed_actor_id, state)
          VALUES ($1, $2, $3, 'pending') RETURNING id",
        follow_ap_id,
        remote.id,
        local.id,
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    let state = AppState::from_pool(pool.clone(), make_config());
    follow_request::approve_or_reject(&state, follow_id, FollowState::Rejected)
        .await
        .unwrap();

    let row = sqlx::query!("SELECT state FROM follow WHERE id = $1", follow_id,)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.state, "rejected");

    let q = sqlx::query!(
        r#"SELECT activity as "activity: sqlx::types::Json<serde_json::Value>"
           FROM delivery_queue WHERE sender_actor_id = $1"#,
        local.id,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(q.activity.0["type"], "Reject");
    assert_eq!(q.activity.0["object"]["id"], follow_ap_id);

    // 再度叩いても pending 以外なので拒否される (idempotent でなく明示エラー)。
    let err = follow_request::approve_or_reject(&state, follow_id, FollowState::Accepted)
        .await
        .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("only `pending` rows"),
        "expected idempotency guard error, got {msg}",
    );
}

// =============================================================================
// Note 本文リモート絵文字の学習 (Issue #328 フォローアップ)
// =============================================================================

/// followee からの Create で Note 本文に Emoji tag があれば `emoji` テーブルに
/// 学習される (リアクション経由の学習とは独立した経路)。テスト環境は
/// media-proxy 未接続 (`socket: "/tmp/x"`) なので `image_key` は `None` に
/// なるが、行自体は作られる (= `emoji_learn::learn_emoji_tag_object` の
/// fetch失敗パス、既存 `inbound_emoji_react_learns_remote_emoji` と同じ検証粒度)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn create_with_emoji_tag_learns_remote_emoji(pool: PgPool) {
    let (_lp, local_pub) = fresh_rsa();
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

    let follow_ap = format!("https://{LOCAL_HOST}/users/{LOCAL_USER}/activities/follow-bob-e1");
    let f = repo::follow::insert_pending(&pool, &follow_ap, local.id, remote.id)
        .await
        .unwrap();
    repo::follow::set_state(&pool, f.id, sakurasato_core::model::FollowState::Accepted)
        .await
        .unwrap();

    let state = AppState::from_pool(pool.clone(), make_config());
    let app = router(state);

    let note_id = "https://remote.test/notes/note-with-emoji";
    let emoji_ap_id = "https://remote.test/emojis/blob_party";
    let body = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://remote.test/users/bob/activities/create-emoji-1",
        "type": "Create",
        "actor": remote.ap_id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": note_id,
            "type": "Note",
            "attributedTo": remote.ap_id,
            "content": "hello :blob_party:",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "published": "2026-05-31T12:00:00Z",
            "tag": [
                {
                    "type": "Emoji",
                    "id": emoji_ap_id,
                    "name": ":blob_party:",
                    "icon": {"type": "Image", "mediaType": "image/png", "url": "https://remote.test/files/blob_party.png"},
                },
            ],
        },
    })
    .to_string();
    let keyid = format!("{}#main-key", remote.ap_id);
    let req = build_signed_post(body.as_bytes(), "/inbox", &remote_priv, &keyid, LOCAL_HOST);

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let learned = repo::emoji::get_by_ap_id(&pool, emoji_ap_id)
        .await
        .unwrap()
        .expect("emoji must be learned from Note.tag");
    assert_eq!(learned.shortcode, "blob_party");
    assert_eq!(learned.host.as_deref(), Some("remote.test"));
    assert!(!learned.is_local);
}

/// Note 本文に複数の Emoji tag があれば全件学習される (reaction 用の
/// content 一致学習との違いを担保する回帰テスト)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn create_with_multiple_emoji_tags_learns_all(pool: PgPool) {
    let (_lp, local_pub) = fresh_rsa();
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

    let follow_ap = format!("https://{LOCAL_HOST}/users/{LOCAL_USER}/activities/follow-bob-e2");
    let f = repo::follow::insert_pending(&pool, &follow_ap, local.id, remote.id)
        .await
        .unwrap();
    repo::follow::set_state(&pool, f.id, sakurasato_core::model::FollowState::Accepted)
        .await
        .unwrap();

    let state = AppState::from_pool(pool.clone(), make_config());
    let app = router(state);

    let note_id = "https://remote.test/notes/note-with-two-emojis";
    let emoji_a = "https://remote.test/emojis/blob_a";
    let emoji_b = "https://remote.test/emojis/blob_b";
    let body = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://remote.test/users/bob/activities/create-emoji-2",
        "type": "Create",
        "actor": remote.ap_id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": note_id,
            "type": "Note",
            "attributedTo": remote.ap_id,
            "content": "hi :blob_a: and :blob_b:",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "published": "2026-05-31T12:00:00Z",
            "tag": [
                {"type": "Emoji", "id": emoji_a, "name": ":blob_a:",
                 "icon": {"type": "Image", "mediaType": "image/png", "url": "https://remote.test/files/blob_a.png"}},
                {"type": "Emoji", "id": emoji_b, "name": ":blob_b:",
                 "icon": {"type": "Image", "mediaType": "image/png", "url": "https://remote.test/files/blob_b.png"}},
            ],
        },
    })
    .to_string();
    let keyid = format!("{}#main-key", remote.ap_id);
    let req = build_signed_post(body.as_bytes(), "/inbox", &remote_priv, &keyid, LOCAL_HOST);

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    assert!(
        repo::emoji::get_by_ap_id(&pool, emoji_a)
            .await
            .unwrap()
            .is_some(),
        "first emoji tag must be learned"
    );
    assert!(
        repo::emoji::get_by_ap_id(&pool, emoji_b)
            .await
            .unwrap()
            .is_some(),
        "second emoji tag must be learned"
    );
}

/// [[claude-review-330]]: Note本文の Emoji tag 学習には上限
/// (`emoji_learn::MAX_EMOJI_TAGS_PER_NOTE` = 32) があり、超過分は学習
/// されない。上限が無いと、未キャッシュの tag ごとに media-proxy fetch を
/// 直列 await するため、悪意ある大量 tag で inbox 処理を長時間ブロック
/// させる増幅型 `DoS` になりうる (最小の Emoji tag は ~150 バイトなので
/// inbox body 上限 1MiB に対し数千件詰め込める)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn create_with_emoji_tags_exceeding_cap_only_learns_up_to_max(pool: PgPool) {
    // MAX_EMOJI_TAGS_PER_NOTE (32) を超える件数を仕込む。
    const TAG_COUNT: usize = 35;
    const EXPECTED_LEARNED: usize = 32;

    let (_lp, local_pub) = fresh_rsa();
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

    let follow_ap = format!("https://{LOCAL_HOST}/users/{LOCAL_USER}/activities/follow-bob-e3");
    let f = repo::follow::insert_pending(&pool, &follow_ap, local.id, remote.id)
        .await
        .unwrap();
    repo::follow::set_state(&pool, f.id, sakurasato_core::model::FollowState::Accepted)
        .await
        .unwrap();

    let state = AppState::from_pool(pool.clone(), make_config());
    let app = router(state);

    let tags: Vec<serde_json::Value> = (0..TAG_COUNT)
        .map(|i| {
            serde_json::json!({
                "type": "Emoji",
                "id": format!("https://remote.test/emojis/cap_{i}"),
                "name": format!(":cap_{i}:"),
                "icon": {"type": "Image", "mediaType": "image/png", "url": format!("https://remote.test/files/cap_{i}.png")},
            })
        })
        .collect();

    let note_id = "https://remote.test/notes/note-with-many-emojis";
    let body = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://remote.test/users/bob/activities/create-emoji-cap",
        "type": "Create",
        "actor": remote.ap_id,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": note_id,
            "type": "Note",
            "attributedTo": remote.ap_id,
            "content": "many emojis",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "published": "2026-05-31T12:00:00Z",
            "tag": tags,
        },
    })
    .to_string();
    let keyid = format!("{}#main-key", remote.ap_id);
    let req = build_signed_post(body.as_bytes(), "/inbox", &remote_priv, &keyid, LOCAL_HOST);

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let mut learned_count = 0;
    for i in 0..TAG_COUNT {
        let ap_id = format!("https://remote.test/emojis/cap_{i}");
        if repo::emoji::get_by_ap_id(&pool, &ap_id)
            .await
            .unwrap()
            .is_some()
        {
            learned_count += 1;
        }
    }
    assert_eq!(
        learned_count, EXPECTED_LEARNED,
        "only up to the per-note cap should be learned"
    );
}

/// Update.object.tag に Emoji があれば学習される。`update_content` による
/// content/summary 更新とは独立した経路であることを確認する。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn update_note_with_emoji_tag_learns_remote_emoji(pool: PgPool) {
    let (_lp, local_pub) = fresh_rsa();
    let (remote_priv, remote_pub) = fresh_rsa();

    let _ = repo::actor::insert(&pool, local_actor(&local_pub, "PRIV"))
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

    let note_ap_id = "https://remote.test/notes/edit-target-emoji";
    repo::note::insert(
        &pool,
        sakurasato_core::repo::note::NewNote {
            ap_id: note_ap_id.into(),
            actor_id: remote.id,
            content: "original".into(),
            language: None,
            in_reply_to_ap_id: None,
            in_reply_to_note_id: None,
            summary: None,
            visibility: sakurasato_core::model::Visibility::Public,
            sensitive: false,
            to_recipients: vec![],
            cc_recipients: vec![],
            attachments: serde_json::Value::Array(vec![]),
            tags: serde_json::Value::Array(vec![]),
            is_local: false,
            url: None,
            source: None,
            published_at: chrono::Utc::now(),
        },
    )
    .await
    .unwrap();

    let state = AppState::from_pool(pool.clone(), make_config());
    let app = router(state);

    let emoji_ap_id = "https://remote.test/emojis/edited_emoji";
    let body = serde_json::json!({
        "id": "https://remote.test/users/bob/activities/update-emoji-1",
        "type": "Update",
        "actor": remote.ap_id,
        "object": {
            "id": note_ap_id,
            "type": "Note",
            "attributedTo": remote.ap_id,
            "content": "edited with :edited_emoji:",
            "updated": "2026-06-01T00:00:00Z",
            "tag": [
                {"type": "Emoji", "id": emoji_ap_id, "name": ":edited_emoji:",
                 "icon": {"type": "Image", "mediaType": "image/png", "url": "https://remote.test/files/edited_emoji.png"}},
            ],
        },
    })
    .to_string();
    let keyid = format!("{}#main-key", remote.ap_id);
    let req = build_signed_post(body.as_bytes(), "/inbox", &remote_priv, &keyid, LOCAL_HOST);

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let after = repo::note::get_by_ap_id(&pool, note_ap_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.content, "edited with :edited_emoji:");

    let learned = repo::emoji::get_by_ap_id(&pool, emoji_ap_id)
        .await
        .unwrap()
        .expect("emoji must be learned from Update.object.tag");
    assert_eq!(learned.shortcode, "edited_emoji");
    assert_eq!(learned.host.as_deref(), Some("remote.test"));
}
