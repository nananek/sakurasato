//! M3a integration tests: spin up the axum router against a real Postgres,
//! seed a local actor, and hit `WebFinger` / `NodeInfo` / actor / outbox /
//! inbox endpoints with `tower::ServiceExt::oneshot`.

#![forbid(unsafe_code)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
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
            summary: Some("hello".into()),
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

    /// 本物の Ed25519 公開鍵 PEM をテスト用に毎回生成する。actor JSON 側で
    /// PEM を multibase に変換するため、MOCK な PEM だと変換に失敗して
    /// `assertionMethod` が omit され、検証ができない。
    pub(super) fn sample_ed25519_public_pem() -> String {
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
        },
        miauth: None,
    }
}

async fn read_json(resp: axum::response::Response) -> serde_json::Value {
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap()
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn webfinger_returns_local_actor(pool: PgPool) {
    repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::routes::router(state);

    let resp = app
        .oneshot(
            Request::get("/.well-known/webfinger?resource=acct:alice@example.test")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp.headers().get(header::CONTENT_TYPE).unwrap();
    assert_eq!(ct, "application/jrd+json");
    let json = read_json(resp).await;
    assert_eq!(json["subject"], "acct:alice@example.test");
    assert_eq!(json["links"][0]["rel"], "self");
    assert_eq!(json["links"][0]["type"], "application/activity+json");
    assert_eq!(json["links"][0]["href"], "https://example.test/users/alice");
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn webfinger_rejects_unknown_host(pool: PgPool) {
    repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::routes::router(state);

    let resp = app
        .oneshot(
            Request::get("/.well-known/webfinger?resource=acct:alice@other.test")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn webfinger_rejects_malformed_resource(pool: PgPool) {
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::routes::router(state);

    let resp = app
        .oneshot(
            Request::get("/.well-known/webfinger?resource=not-an-acct")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn nodeinfo_discovery_links_to_v2_1(pool: PgPool) {
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::routes::router(state);

    let resp = app
        .oneshot(
            Request::get("/.well-known/nodeinfo")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    let link = &json["links"][0];
    assert_eq!(
        link["rel"],
        "http://nodeinfo.diaspora.software/ns/schema/2.1"
    );
    assert_eq!(link["href"], "https://example.test/nodeinfo/2.1");
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn nodeinfo_v2_1_reports_sakurasato(pool: PgPool) {
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::routes::router(state);

    let resp = app
        .oneshot(Request::get("/nodeinfo/2.1").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    assert_eq!(json["software"]["name"], "sakurasato");
    assert_eq!(json["protocols"][0], "activitypub");
    assert_eq!(json["usage"]["users"]["total"], 1);
    assert_eq!(json["openRegistrations"], false);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn actor_json_redacts_private_key(pool: PgPool) {
    repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::routes::router(state);

    let resp = app
        .oneshot(
            Request::get("/users/alice")
                .header("accept", "application/activity+json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/activity+json"
    );
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let text = std::str::from_utf8(&body).unwrap().to_owned();
    assert!(text.contains("\"type\":\"Person\""), "got: {text}");
    assert!(text.contains("\"preferredUsername\":\"alice\""));
    assert!(text.contains("publicKeyPem"), "must include public key");
    assert!(
        !text.contains("private_key_pem"),
        "must NOT include private key field name"
    );
    assert!(
        !text.contains("BEGIN PRIVATE KEY"),
        "must NOT include private key body"
    );
    assert!(
        !text.contains("MOCK-ED"),
        "must NOT leak Ed25519 private key marker: {text}",
    );
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn actor_json_publishes_ed25519_assertion_method(pool: PgPool) {
    repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::routes::router(state);

    let resp = app
        .oneshot(
            Request::get("/users/alice")
                .header("accept", "application/activity+json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;

    // @context に Multikey 用 URI が積まれていること。
    let ctx = json["@context"].as_array().expect("context is array");
    assert!(
        ctx.iter()
            .any(|v| v == "https://w3id.org/security/multikey/v1"),
        "context must include multikey vocab: {ctx:?}",
    );

    // assertionMethod: Multikey 1 件、Ed25519 鍵 ID と multibase 値を含む。
    let am = json["assertionMethod"]
        .as_array()
        .expect("assertionMethod should be present");
    assert_eq!(am.len(), 1, "expected exactly one Multikey: {am:?}");
    let entry = &am[0];
    assert_eq!(entry["type"], "Multikey");
    assert_eq!(entry["id"], "https://example.test/users/alice#ed25519-key");
    assert_eq!(entry["controller"], "https://example.test/users/alice");
    let mb = entry["publicKeyMultibase"]
        .as_str()
        .expect("publicKeyMultibase must be a string");
    assert!(
        mb.starts_with('z'),
        "publicKeyMultibase must be base58btc-prefixed: {mb}",
    );
    // base58btc("ed 01" || 32-byte) は概ね 48 文字 + 'z'。
    assert!(
        (48..=52).contains(&mb.len()),
        "unexpected multibase length: {mb}",
    );
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn actor_json_omits_assertion_method_when_no_ed25519(pool: PgPool) {
    // Ed25519 鍵を持たない actor (旧 M3a の local actor 等) では
    // assertionMethod を omit し、multikey context も載せない。
    let mut new = common::sample_local_actor("bob", "example.test");
    new.ed25519_public_key_id = None;
    new.ed25519_public_key_pem = None;
    new.ed25519_private_key_pem = None;
    repo::actor::insert(&pool, new).await.unwrap();
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::routes::router(state);

    let resp = app
        .oneshot(
            Request::get("/users/bob")
                .header("accept", "application/activity+json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    assert!(
        json.get("assertionMethod").is_none(),
        "assertionMethod must be omitted when actor has no Ed25519 key: {json}",
    );
    let ctx = json["@context"].as_array().expect("context is array");
    assert!(
        !ctx.iter()
            .any(|v| v == "https://w3id.org/security/multikey/v1"),
        "multikey context must be omitted alongside assertionMethod: {ctx:?}",
    );
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn actor_json_emits_manually_approves_followers(pool: PgPool) {
    // Issue #66 / M12: 鍵アカフラグは actor JSON に常時 emit する。
    // false (= 通常アカ) と true (= 鍵アカ) の両方を確認。
    let mut unlocked = common::sample_local_actor("uno", "example.test");
    unlocked.manually_approves_followers = false;
    let mut locked = common::sample_local_actor("locked", "example.test");
    locked.manually_approves_followers = true;
    repo::actor::insert(&pool, unlocked).await.unwrap();
    repo::actor::insert(&pool, locked).await.unwrap();
    let state =
        sakurasato_server::state::AppState::from_pool(pool.clone(), make_config("example.test"));
    let app = sakurasato_server::routes::router(state);

    let resp_u = app
        .clone()
        .oneshot(
            Request::get("/users/uno")
                .header("accept", "application/activity+json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body_u = read_json(resp_u).await;
    assert_eq!(
        body_u["manuallyApprovesFollowers"], false,
        "unlocked actor emits manuallyApprovesFollowers=false: {body_u}",
    );

    let resp_l = app
        .oneshot(
            Request::get("/users/locked")
                .header("accept", "application/activity+json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body_l = read_json(resp_l).await;
    assert_eq!(
        body_l["manuallyApprovesFollowers"], true,
        "locked actor emits manuallyApprovesFollowers=true: {body_l}",
    );

    // @context に `manuallyApprovesFollowers` の alias オブジェクトが
    // 含まれていること (strict JSON-LD processor 対策)。
    let ctx = body_l["@context"].as_array().expect("context is array");
    let has_alias = ctx.iter().any(|v| {
        v.as_object()
            .and_then(|m| m.get("manuallyApprovesFollowers"))
            .and_then(|x| x.as_str())
            == Some("as:manuallyApprovesFollowers")
    });
    assert!(
        has_alias,
        "context must alias manuallyApprovesFollowers to as:manuallyApprovesFollowers: {ctx:?}",
    );
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn actor_json_404_for_unknown_user(pool: PgPool) {
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::routes::router(state);

    let resp = app
        .oneshot(Request::get("/users/ghost").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn outbox_returns_empty_ordered_collection(pool: PgPool) {
    repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::routes::router(state);

    let resp = app
        .oneshot(
            Request::get("/users/alice/outbox")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = read_json(resp).await;
    assert_eq!(json["type"], "OrderedCollection");
    assert_eq!(json["totalItems"], 0);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn inbox_rejects_unsigned_post_with_400(pool: PgPool) {
    // M3a までは placeholder で 202 を返していたが、M3b-2 で署名検証が
    // extractor として配線された。Signature ヘッダ無しのリクエストは
    // 「ActivityPub inbox の仕様を満たしていない」として 400 で弾く。
    // 詳細な署名検証の網羅は crates/server/src/inbox_signature_tests.rs。
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::routes::router(state);

    let resp = app
        .oneshot(
            Request::post("/inbox")
                .header("content-type", "application/activity+json")
                .body(Body::from("{\"type\":\"Create\"}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
