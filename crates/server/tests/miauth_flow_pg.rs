//! M14 #158 — `MiAuth` 認証フロー (session register + check polling + `/api/i`)
//! の統合テスト (= 親 issue #150)。
//!
//! `#[sqlx::test]` で per-test DB を切り、`miauth::router` を `tower::ServiceExt::oneshot`
//! 経由で叩く。Unix socket は立てず axum router の挙動だけを確認する
//! (= `local_api_pg.rs` と同じ流儀)。
//!
//! ## カバレッジ (= 親 issue #158 Acceptance criteria)
//!
//! 1. `GET /miauth/{uuid}?name=&permission=&callback=` が pending session を
//!    登録し、text/html で CLI 指示テキストを返す
//! 2. `POST /api/miauth/{uuid}/check` を pending 中に叩くと
//!    `200 {ok: false, token: null}`
//! 3. CLI `approve` 経路 (= `repo::miauth::approve_session`) 後の check は
//!    `200 {ok: true, token, user}` で `miauth_token` 行が作られ、session が
//!    `consumed` に倒れる
//! 4. 同 session 2 回目の check は **同じ raw token** を返す (冪等)
//! 5. `rejected` / `expired` / `invalid UUID` の check は `404`
//! 6. `POST /api/i { i: <token> }` が `MissUser` JSON を返す
//! 7. `Authorization: Bearer <token>` でも `/api/i` が通る (= 互換 fallback)
//! 8. scope 不足の token は `/api/i` で 403
//! 9. 不正 / 不在 token は `/api/i` で 401
//!
//! ## AGPL discipline
//!
//! 本テスト群は Sakurasato 側 (= MIT) の router/handler/repo を叩くだけで、
//! Misskey 本体 (= AGPL-3.0) は参照しない。実 Misskey との wire-compat parity
//! は `tests/federation/test_miauth_flow_parity.py` (= pytest + misskey-py) で
//! 別経路で確認する ── [`scripts/federation-test/pytest.sh misskey`] が
//! 立てる stack 経由で実行される。

#![forbid(unsafe_code)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use sakurasato_core::repo;
use sakurasato_core::repo::actor::NewActor;
use sakurasato_server::miauth;
use sakurasato_server::state::AppState;
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
            // `/miauth/{uuid}` handler が session ttl を参照するため `Some` で
            // 入れる ── 値はテスト内で寿命を切るかどうか分岐する。
            miauth: Some(MiAuthConfig {
                listen: "unix:/tmp/miauth.sock".into(),
                session_ttl_secs: 600,
            }),
        }
    }
}

/// `local_api_pg.rs` と同形の最小 local actor を 1 つ仕込む。
/// `/api/i` の `MissUser` 構築でこの actor が引かれる。
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

async fn read_body_bytes(resp: axum::response::Response) -> Vec<u8> {
    resp.into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

async fn read_json(resp: axum::response::Response) -> serde_json::Value {
    let body = read_body_bytes(resp).await;
    serde_json::from_slice(&body).expect("response body must be JSON")
}

/// `GET /miauth/{uuid}` で pending session を登録。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn landing_inserts_pending_session(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = AppState::from_pool(
        pool.clone(),
        common::make_config("sakurasato.test", "alice"),
    );
    let app = miauth::router(state);

    let uuid = Uuid::new_v4();
    let path = format!(
        "/miauth/{uuid}?name=Milktea&permission=read:account,write:reactions&callback=https://app.test/cb"
    );
    let resp = app
        .oneshot(Request::get(&path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "text/html; charset=utf-8"
    );
    let body = String::from_utf8(read_body_bytes(resp).await).unwrap();
    // CLI 指示テキストが本文に含まれる。
    assert!(
        body.contains("sakurasato-server miauth approve"),
        "landing must contain CLI hint, got: {body}"
    );
    assert!(body.contains(&uuid.to_string()));
    // session 行が登録されている。
    let row = repo::miauth::get_session(&pool, uuid)
        .await
        .unwrap()
        .expect("session row should exist");
    assert_eq!(row.app_name, "Milktea");
    assert_eq!(row.permissions.0, vec!["read:account", "write:reactions"]);
    assert_eq!(row.callback_url.as_deref(), Some("https://app.test/cb"));
    assert_eq!(row.state, "pending");
}

/// `GET /miauth/{uuid}` の query が空のときは scope ゼロ + `app_name=unknown app`。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn landing_with_empty_query_records_empty_scope(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = AppState::from_pool(
        pool.clone(),
        common::make_config("sakurasato.test", "alice"),
    );
    let app = miauth::router(state);

    let uuid = Uuid::new_v4();
    let path = format!("/miauth/{uuid}");
    let resp = app
        .oneshot(Request::get(&path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let row = repo::miauth::get_session(&pool, uuid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.app_name, "unknown app");
    assert!(row.permissions.0.is_empty());
}

/// 不正 UUID は 400。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn landing_with_invalid_uuid_returns_400(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = AppState::from_pool(pool, common::make_config("sakurasato.test", "alice"));
    let app = miauth::router(state);
    let resp = app
        .oneshot(
            Request::get("/miauth/not-a-uuid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// pending 状態の check は 200 + `{ok: false, token: null, user: null}`。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn check_pending_returns_polling_response(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = AppState::from_pool(
        pool.clone(),
        common::make_config("sakurasato.test", "alice"),
    );
    let app = miauth::router(state.clone());

    let uuid = Uuid::new_v4();
    // pending session を直接 INSERT して landing をスキップ。
    repo::miauth::insert_session(
        &pool,
        repo::miauth::NewMiAuthSession {
            uuid,
            app_name: "TestApp".into(),
            callback_url: None,
            permissions: vec!["read:account".into()],
            expires_at: chrono::Utc::now() + chrono::Duration::seconds(600),
        },
    )
    .await
    .unwrap();

    let path = format!("/api/miauth/{uuid}/check");
    let resp = app
        .oneshot(Request::post(&path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_json(resp).await;
    assert_eq!(body["ok"], false);
    assert!(body["token"].is_null());
    assert!(body["user"].is_null());
}

/// CLI approve 後の check は token + self `MeDetailed` (= `/api/i` と同形) を返し、
/// session が consumed に倒れる。#150 (Aria fix) で最小 `MissUser` から昇格。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn check_after_approve_returns_token_and_user(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = AppState::from_pool(
        pool.clone(),
        common::make_config("sakurasato.test", "alice"),
    );
    let app = miauth::router(state.clone());

    let uuid = Uuid::new_v4();
    repo::miauth::insert_session(
        &pool,
        repo::miauth::NewMiAuthSession {
            uuid,
            app_name: "TestApp".into(),
            callback_url: None,
            permissions: vec!["read:account".into()],
            expires_at: chrono::Utc::now() + chrono::Duration::seconds(600),
        },
    )
    .await
    .unwrap();

    // CLI 経路: pending → approved CAS のみ。
    let rows = repo::miauth::approve_session(&pool, uuid, &["read:account".into()])
        .await
        .unwrap();
    assert_eq!(rows, 1);

    // approved 状態への check ── token 発行 + consumed への CAS + self `MeDetailed` を返す。
    let path = format!("/api/miauth/{uuid}/check");
    let resp = app
        .clone()
        .oneshot(Request::post(&path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_json(resp).await;
    assert_eq!(body["ok"], true);
    let raw = body["token"]
        .as_str()
        .expect("token must be string")
        .to_string();
    assert!(!raw.is_empty());
    assert_eq!(body["user"]["username"], "alice");
    assert!(body["user"]["host"].is_null(), "local user host is null");
    assert_eq!(body["user"]["isLocked"], false);
    assert_eq!(body["user"]["followersCount"], 0);
    assert_eq!(body["user"]["followingCount"], 0);
    assert_eq!(body["user"]["notesCount"], 0);
    // #150 (Aria fix): check の `user` は `/api/i` と同じ full `MeDetailed`。
    // Aria (misskey_dart) は check レスポンスの `user` を self `MeDetailed` として
    // parse し、以下の required bool を cast する ── 欠落すると `null as bool` で
    // `type 'Null' is not a subtype of type 'bool'` crash する。最小 `MissUser`
    // (= UserLite) への回帰を防ぐため、UserDetailed / MeDetailed 双方の必須 bool
    // が存在し bool 型であることを固定する。
    for key in [
        "isBot",
        "isCat",
        "isSilenced",
        "isSuspended",
        "publicReactions",
        "isAdmin",
        "isModerator",
        "hasUnreadNotification",
    ] {
        assert!(
            body["user"][key].is_boolean(),
            "check user must carry MeDetailed required bool {key:?} (Aria crash otherwise); got {:?}",
            body["user"][key]
        );
    }
    // `MeDetailed` 固有の object / array も存在する (= `/api/i` と同形)。
    assert!(
        body["user"]["policies"].is_object(),
        "check user must carry MeDetailed policies object"
    );
    assert!(
        body["user"]["roles"].is_array(),
        "check user must carry MeDetailed roles array"
    );

    // session が consumed に倒れていて、token 行が作られている。
    let row = repo::miauth::get_session(&pool, uuid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, "consumed");
    assert!(row.issued_token_id.is_some());
    assert_eq!(row.raw_token_for_polling.as_deref(), Some(raw.as_str()));

    // ── 冪等 ── 同 session への 2 回目の check は同じ raw token を返す。
    let resp2 = app
        .oneshot(Request::post(&path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);
    let body2 = read_json(resp2).await;
    assert_eq!(body2["ok"], true);
    assert_eq!(body2["token"], raw, "2nd check must return same raw token");
    assert_eq!(body2["user"]["username"], "alice");
}

/// rejected session への check は 404。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn check_rejected_session_returns_404(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = AppState::from_pool(
        pool.clone(),
        common::make_config("sakurasato.test", "alice"),
    );
    let app = miauth::router(state);

    let uuid = Uuid::new_v4();
    repo::miauth::insert_session(
        &pool,
        repo::miauth::NewMiAuthSession {
            uuid,
            app_name: "TestApp".into(),
            callback_url: None,
            permissions: vec![],
            expires_at: chrono::Utc::now() + chrono::Duration::seconds(600),
        },
    )
    .await
    .unwrap();
    let rows = repo::miauth::reject_session(&pool, uuid).await.unwrap();
    assert_eq!(rows, 1);

    let resp = app
        .oneshot(
            Request::post(format!("/api/miauth/{uuid}/check"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// 期限切れ pending session への check は best-effort sweep で expired に倒れて 404。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn check_expired_session_returns_404(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = AppState::from_pool(
        pool.clone(),
        common::make_config("sakurasato.test", "alice"),
    );
    let app = miauth::router(state);

    let uuid = Uuid::new_v4();
    // 既に過期した pending session を直接仕込む。
    repo::miauth::insert_session(
        &pool,
        repo::miauth::NewMiAuthSession {
            uuid,
            app_name: "TestApp".into(),
            callback_url: None,
            permissions: vec![],
            expires_at: chrono::Utc::now() - chrono::Duration::seconds(60),
        },
    )
    .await
    .unwrap();

    let resp = app
        .oneshot(
            Request::post(format!("/api/miauth/{uuid}/check"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    // sweep が走って expired に倒れていることを確認。
    let row = repo::miauth::get_session(&pool, uuid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, "expired");
}

/// 不在 session UUID への check は 404。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn check_unknown_session_returns_404(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = AppState::from_pool(pool, common::make_config("sakurasato.test", "alice"));
    let app = miauth::router(state);
    let uuid = Uuid::new_v4();
    let resp = app
        .oneshot(
            Request::post(format!("/api/miauth/{uuid}/check"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// `POST /api/i { i: <token> }` で `MissUser` が返る。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn api_i_with_body_token_returns_miss_user(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = AppState::from_pool(
        pool.clone(),
        common::make_config("sakurasato.test", "alice"),
    );
    let app = miauth::router(state);

    // full flow を E2E で走らせて token を取り出す。
    let uuid = Uuid::new_v4();
    repo::miauth::insert_session(
        &pool,
        repo::miauth::NewMiAuthSession {
            uuid,
            app_name: "TestApp".into(),
            callback_url: None,
            permissions: vec!["read:account".into()],
            expires_at: chrono::Utc::now() + chrono::Duration::seconds(600),
        },
    )
    .await
    .unwrap();
    repo::miauth::approve_session(&pool, uuid, &["read:account".into()])
        .await
        .unwrap();
    let resp = app
        .clone()
        .oneshot(
            Request::post(format!("/api/miauth/{uuid}/check"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = read_json(resp).await;
    let raw = body["token"].as_str().unwrap().to_string();

    // `/api/i { i: raw }` で叩く。
    let body = serde_json::json!({"i": raw});
    let resp = app
        .oneshot(
            Request::post("/api/i")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let miss = read_json(resp).await;
    assert_eq!(miss["username"], "alice");
    assert!(miss["host"].is_null());
    assert_eq!(miss["name"], "Alice");
    assert_eq!(miss["avatarUrl"], "https://cdn.test/avatar.webp");
    assert_eq!(miss["isLocked"], false);
    // id is stringified i64.
    let id_str = miss["id"].as_str().expect("id must be string");
    assert!(id_str.parse::<i64>().is_ok());
}

/// `Authorization: Bearer <token>` で `/api/i` が通る (= 互換 fallback)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn api_i_with_bearer_header_returns_miss_user(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = AppState::from_pool(
        pool.clone(),
        common::make_config("sakurasato.test", "alice"),
    );
    let app = miauth::router(state);

    // token を発行する近道: full flow を走らせる。
    let uuid = Uuid::new_v4();
    repo::miauth::insert_session(
        &pool,
        repo::miauth::NewMiAuthSession {
            uuid,
            app_name: "Iceshrimp-like".into(),
            callback_url: None,
            permissions: vec!["read:account".into()],
            expires_at: chrono::Utc::now() + chrono::Duration::seconds(600),
        },
    )
    .await
    .unwrap();
    repo::miauth::approve_session(&pool, uuid, &["read:account".into()])
        .await
        .unwrap();
    let resp = app
        .clone()
        .oneshot(
            Request::post(format!("/api/miauth/{uuid}/check"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = read_json(resp).await;
    let raw = body["token"].as_str().unwrap().to_string();

    // body 無し + Bearer ヘッダだけで叩く。
    let resp = app
        .oneshot(
            Request::post("/api/i")
                .header(header::AUTHORIZATION, format!("Bearer {raw}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let miss = read_json(resp).await;
    assert_eq!(miss["username"], "alice");
}

/// scope 不足 (= `read:account` を持たない token) は `/api/i` で 403。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn api_i_without_read_account_scope_returns_403(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = AppState::from_pool(
        pool.clone(),
        common::make_config("sakurasato.test", "alice"),
    );
    let app = miauth::router(state);

    let uuid = Uuid::new_v4();
    repo::miauth::insert_session(
        &pool,
        repo::miauth::NewMiAuthSession {
            uuid,
            app_name: "TestApp".into(),
            callback_url: None,
            // `write:reactions` だけで read:account が無い。
            permissions: vec!["write:reactions".into()],
            expires_at: chrono::Utc::now() + chrono::Duration::seconds(600),
        },
    )
    .await
    .unwrap();
    repo::miauth::approve_session(&pool, uuid, &["write:reactions".into()])
        .await
        .unwrap();
    let resp = app
        .clone()
        .oneshot(
            Request::post(format!("/api/miauth/{uuid}/check"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = read_json(resp).await;
    let raw = body["token"].as_str().unwrap().to_string();

    let body_in = serde_json::json!({"i": raw});
    let resp = app
        .oneshot(
            Request::post("/api/i")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body_in).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// token 不在 / 不正は 401。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn api_i_without_token_returns_401(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = AppState::from_pool(pool, common::make_config("sakurasato.test", "alice"));
    let app = miauth::router(state);

    // body 無し / ヘッダ無し。
    let resp = app
        .clone()
        .oneshot(Request::post("/api/i").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // body あり / token 不在。
    let body = serde_json::json!({"i": "nonexistent-raw-token"});
    let resp = app
        .oneshot(
            Request::post("/api/i")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// `MissUser` の id / host / camelCase 命名が parity test と整合する。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn miss_user_schema_uses_camel_case_with_required_keys(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = AppState::from_pool(
        pool.clone(),
        common::make_config("sakurasato.test", "alice"),
    );
    let app = miauth::router(state);

    let uuid = Uuid::new_v4();
    repo::miauth::insert_session(
        &pool,
        repo::miauth::NewMiAuthSession {
            uuid,
            app_name: "Schema-check".into(),
            callback_url: None,
            permissions: vec!["read:account".into()],
            expires_at: chrono::Utc::now() + chrono::Duration::seconds(600),
        },
    )
    .await
    .unwrap();
    repo::miauth::approve_session(&pool, uuid, &["read:account".into()])
        .await
        .unwrap();
    let resp = app
        .oneshot(
            Request::post(format!("/api/miauth/{uuid}/check"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = read_json(resp).await;
    let user = body["user"].as_object().expect("user object");
    // Misskey UserLite + UserDetailed (minimum) で必須なフィールド 9 種。
    for k in [
        "id",
        "name",
        "username",
        "host",
        "avatarUrl",
        "isLocked",
        "followersCount",
        "followingCount",
        "notesCount",
    ] {
        assert!(
            user.contains_key(k),
            "MissUser must contain key {k:?}, got keys: {:?}",
            user.keys().collect::<Vec<_>>()
        );
    }
    // 型確認。
    assert!(user["id"].is_string());
    assert!(user["username"].is_string());
    assert!(user["host"].is_null());
    assert!(user["isLocked"].is_boolean());
    assert!(user["followersCount"].is_number());
}

/// `/healthz` は 200 + "ok"。M14 #157 で入った liveness probe が #158 後も
/// 維持されていることの回帰確認。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn healthz_remains_alive(pool: PgPool) {
    let state = AppState::from_pool(pool, common::make_config("sakurasato.test", "alice"));
    let app = miauth::router(state);
    let resp = app
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(read_body_bytes(resp).await).unwrap();
    assert_eq!(body, "ok");
}

/// **#158 設計判断**: `MiAuthSessionState` の状態機械が `pending → approved
/// → consumed` を通る間、`raw_token_for_polling` の値が一貫していることを
/// 確認。`auth::validate_token_raw` で hash 経路の整合性も担保する。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn raw_token_matches_hash_path(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = AppState::from_pool(
        pool.clone(),
        common::make_config("sakurasato.test", "alice"),
    );
    let app = miauth::router(state.clone());

    let uuid = Uuid::new_v4();
    repo::miauth::insert_session(
        &pool,
        repo::miauth::NewMiAuthSession {
            uuid,
            app_name: "HashCheck".into(),
            callback_url: None,
            permissions: vec!["read:account".into()],
            expires_at: chrono::Utc::now() + chrono::Duration::seconds(600),
        },
    )
    .await
    .unwrap();
    repo::miauth::approve_session(&pool, uuid, &["read:account".into()])
        .await
        .unwrap();
    let resp = app
        .oneshot(
            Request::post(format!("/api/miauth/{uuid}/check"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = read_json(resp).await;
    let raw = body["token"].as_str().unwrap().to_string();

    // raw → hash で auth helper の lookup が当たる。
    let row = sakurasato_server::miauth::auth::validate_token_raw(&state, &raw)
        .await
        .expect("hash lookup must find the token row");
    // session の issued_token_id と一致。
    let session = repo::miauth::get_session(&pool, uuid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(session.issued_token_id, Some(row.id));
}

// ─── M14 #170: /api/i が MeDetailed 形を返す ──────────────────────────────

/// `/api/i` のレスポンスに `MeDetailed` 必須フィールドが揃う (= Aria 等の
/// self profile 描画を成立させるため、`MissUser` 最小サブセットでは足りない
/// 件への対処)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn api_i_returns_me_detailed_shape(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = AppState::from_pool(
        pool.clone(),
        common::make_config("sakurasato.test", "alice"),
    );
    let app = miauth::router(state);

    // token を full flow 経由で発行する。
    let uuid = Uuid::new_v4();
    repo::miauth::insert_session(
        &pool,
        repo::miauth::NewMiAuthSession {
            uuid,
            app_name: "TestApp".into(),
            callback_url: None,
            permissions: vec!["read:account".into()],
            expires_at: chrono::Utc::now() + chrono::Duration::seconds(600),
        },
    )
    .await
    .unwrap();
    repo::miauth::approve_session(&pool, uuid, &["read:account".into()])
        .await
        .unwrap();
    let resp = app
        .clone()
        .oneshot(
            Request::post(format!("/api/miauth/{uuid}/check"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let raw = read_json(resp).await["token"].as_str().unwrap().to_string();

    // `/api/i` を叩いて MeDetailed shape を確認。
    let body = serde_json::json!({"i": raw});
    let resp = app
        .oneshot(
            Request::post("/api/i")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let me = read_json(resp).await;

    // UserLite 部分 (= 既存)。
    assert_eq!(me["username"], "alice");
    assert!(me["host"].is_null());
    assert_eq!(me["name"], "Alice");
    assert_eq!(me["avatarUrl"], "https://cdn.test/avatar.webp");
    assert_eq!(me["isLocked"], false);

    // UserDetailed 部分 (= #170 で `/api/i` も含むよう拡張)。
    assert!(me["createdAt"].is_string(), "createdAt must be string");
    assert_eq!(me["description"], "hello");
    assert!(me["bannerUrl"].is_null());
    assert_eq!(me["isBot"], false);
    assert_eq!(me["isCat"], false);

    // MeDetailed 専用 (= Aria が要求しているとみられる field 群)。
    assert_eq!(me["isAdmin"], false);
    assert_eq!(me["isModerator"], false);
    assert_eq!(me["isSilenced"], false);
    assert_eq!(me["isSuspended"], false);
    assert_eq!(me["isExplorable"], true);
    assert_eq!(me["mfmEnabled"], true);
    assert_eq!(me["onlineStatus"], "unknown");

    // 配列系。
    assert!(me["roles"].as_array().unwrap().is_empty());
    assert!(me["mutedWords"].as_array().unwrap().is_empty());
    assert!(me["pinnedNoteIds"].as_array().unwrap().is_empty());

    // object 系。
    assert!(me["emojis"].is_object());
    assert!(
        me["policies"].is_object(),
        "policies must be an object (= same shape as /api/meta.policies)"
    );

    // `/api/meta.policies` と同じ shape ── 代表的な field の存在を確認。
    let policies = &me["policies"];
    assert!(policies["maxFileSizeMb"].is_number());
    assert_eq!(policies["canPublicNote"], true);

    // Me 専用 (= 通常の users/show には乗らない field)。
    assert!(me["email"].is_null());
    assert_eq!(me["emailVerified"], false);
    assert_eq!(me["twoFactorEnabled"], false);
    assert_eq!(me["securityKeys"], false);

    // M14 #174: misskey-dart MeDetailed の required bool 13 件すべて存在。
    // 漏れると `_$MeDetailedFromJson` で Aria が crash する。
    for key in [
        "injectFeaturedNote",
        "receiveAnnouncementEmail",
        "autoSensitive",
        "carefulBot",
        "noCrawle",
        "isDeleted",
        "hasUnreadSpecifiedNotes",
        "hasUnreadMentions",
        "hasUnreadAnnouncement",
        "hasUnreadAntenna",
        "hasUnreadChannel",
        "hasUnreadNotification",
        "hasPendingReceivedFollowRequest",
    ] {
        assert!(
            me[key].is_boolean(),
            "MeDetailed.{key} must be a boolean (= misskey-dart required field); got {:?}",
            me[key],
        );
        assert_eq!(me[key], false, "{key} must be false for お一人様 server");
    }

    // M14 #174: required List/int 3 件。
    assert!(
        me["emailNotificationTypes"].is_array(),
        "emailNotificationTypes must be an array"
    );
    assert!(
        me["achievements"].is_array(),
        "achievements must be an array"
    );
    assert!(
        me["loggedInDays"].is_number(),
        "loggedInDays must be a number; got {:?}",
        me["loggedInDays"]
    );
}

// ─── M14 #174: avatarUrl は icon_url が None でも non-null ─────────────────

/// `actor.icon_url` が `None` でも `/api/i` の `avatarUrl` は **non-null
/// string** (= identicon URL fallback)。misskey-dart `UserLite.avatarUrl:
/// Uri` は non-null required で、`null` だと Aria が crash する。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn api_i_avatar_url_non_null_when_icon_url_missing(pool: PgPool) {
    // icon_url を None で local actor を仕込む (= seed_local_actor を
    // 借りずに直接 NewActor を組み立てる)。
    let host = "sakurasato.test";
    let user = "alice";
    let ap_id = format!("https://{host}/users/{user}");
    let new = sakurasato_core::repo::actor::NewActor {
        ap_id: ap_id.clone(),
        preferred_username: user.into(),
        host: host.into(),
        display_name: Some("Alice".into()),
        summary: None,
        icon_url: None, // ← 重要: avatar 未設定
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
    let actor_id = sakurasato_core::repo::actor::insert(&pool, new)
        .await
        .expect("seed actor")
        .id;

    let state = AppState::from_pool(pool.clone(), common::make_config(host, user));
    let app = miauth::router(state);
    let uuid = Uuid::new_v4();
    repo::miauth::insert_session(
        &pool,
        repo::miauth::NewMiAuthSession {
            uuid,
            app_name: "TestApp".into(),
            callback_url: None,
            permissions: vec!["read:account".into()],
            expires_at: chrono::Utc::now() + chrono::Duration::seconds(600),
        },
    )
    .await
    .unwrap();
    repo::miauth::approve_session(&pool, uuid, &["read:account".into()])
        .await
        .unwrap();
    let resp = app
        .clone()
        .oneshot(
            Request::post(format!("/api/miauth/{uuid}/check"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let raw = read_json(resp).await["token"].as_str().unwrap().to_string();

    let body = serde_json::json!({"i": raw});
    let resp = app
        .oneshot(
            Request::post("/api/i")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let me = read_json(resp).await;

    // **non-null** であること。identicon URL の format も確認。
    assert!(
        me["avatarUrl"].is_string(),
        "avatarUrl must be a JSON string, not null; got {:?}",
        me["avatarUrl"]
    );
    let url = me["avatarUrl"].as_str().unwrap();
    assert!(
        url.starts_with("https://"),
        "avatarUrl must be a valid https URL; got {url}"
    );
    assert!(
        url.contains("/identicon/"),
        "avatarUrl must be the identicon URL fallback; got {url}"
    );
    assert!(
        url.contains(&actor_id.to_string()),
        "identicon URL must contain actor id; got {url}"
    );
}

// ─── M14 #176: /api/endpoints (Aria emoji picker 経由判定) ─────────────────

/// `POST /api/endpoints` が top-level JSON array で endpoint 名一覧を返す
/// (= Aria が `endpoints.contains('emojis')` で `/api/emojis` を使うか判定する
/// 経路、`misskey-dart::Misskey.endpoints()` 互換)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn api_endpoints_returns_top_level_array_with_emojis(pool: PgPool) {
    let _ = seed_local_actor(&pool, "sakurasato.test", "alice").await;
    let state = AppState::from_pool(
        pool.clone(),
        common::make_config("sakurasato.test", "alice"),
    );
    let app = miauth::router(state);

    let resp = app
        .oneshot(
            Request::post("/api/endpoints")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(b"{}".to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_json(resp).await;

    // **top-level array** (= envelope なし、misskey-dart の
    // `apiService.post<List>("endpoints", {})` 互換)。
    let arr = body.as_array().expect("response must be a JSON array");
    assert!(!arr.is_empty(), "endpoints array must not be empty");

    // 全要素が string。
    for item in arr {
        assert!(
            item.is_string(),
            "each endpoint must be a JSON string; got {item:?}"
        );
    }

    // **emojis が含まれる** ── Aria の emoji picker 経路で必須。
    let names: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
    assert!(
        names.contains(&"emojis"),
        "endpoints must contain 'emojis' (= Aria emoji picker dependency); got {names:?}"
    );

    // 主要 endpoint が揃っている。
    for required in &["meta", "i", "notes/timeline", "users/show", "endpoints"] {
        assert!(
            names.contains(required),
            "endpoints must contain '{required}'; got {names:?}"
        );
    }
}
