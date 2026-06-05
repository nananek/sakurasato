//! M4 PR1 統合テスト: `GET /notes/{id}` (Note パーマリンク HTML)。
//!
//! - 未知 id / remote-only note は 404。
//! - 既知 local note は 200 + `text/html; charset=utf-8`、本文に HTML escape
//!   された content が含まれる (PR1 はサニタイザ未実装で content も escape)。

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

    pub(super) fn sample_remote_actor(username: &str, host: &str) -> NewActor {
        let mut a = sample_local_actor(username, host);
        a.is_local = false;
        // remote actor は秘密鍵を持たない。
        a.private_key_pem = None;
        a.ed25519_private_key_pem = None;
        a
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

async fn insert_note(
    pool: &PgPool,
    actor_id: i64,
    host: &str,
    ap_suffix: &str,
    content: &str,
    is_local: bool,
) -> i64 {
    insert_note_with_visibility(
        pool,
        actor_id,
        host,
        ap_suffix,
        content,
        is_local,
        Visibility::Public,
    )
    .await
}

/// **意図的な簡略化**: `to_recipients` は visibility に関わらず常に `as:Public`
/// を入れる。permalink の visibility filter は `note.visibility` カラムを見る
/// 設計で、本 helper は filter の挙動だけ検証する用途。`to_recipients` ベースの
/// 判定 (= AS2 audience の解釈) を直接検証するテストが必要になった時点で、
/// この helper を分ける or 引数化する。
async fn insert_note_with_visibility(
    pool: &PgPool,
    actor_id: i64,
    host: &str,
    ap_suffix: &str,
    content: &str,
    is_local: bool,
    visibility: Visibility,
) -> i64 {
    let ap_id = format!("https://{host}/notes/{ap_suffix}");
    let row = repo::note::insert(
        pool,
        sakurasato_core::repo::note::NewNote {
            ap_id,
            actor_id,
            content: content.into(),
            language: Some("ja".into()),
            in_reply_to_ap_id: None,
            in_reply_to_note_id: None,
            summary: None,
            visibility,
            sensitive: false,
            to_recipients: vec!["https://www.w3.org/ns/activitystreams#Public".into()],
            cc_recipients: vec![],
            attachments: serde_json::json!([]),
            tags: serde_json::json!([]),
            is_local,
            url: None,
            published_at: chrono::Utc::now(),
        },
    )
    .await
    .unwrap();
    row.id
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn permalink_404_for_unknown_id(pool: PgPool) {
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::routes::router(state);

    let resp = app
        .oneshot(Request::get("/notes/999999").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn permalink_404_for_remote_note(pool: PgPool) {
    let actor = repo::actor::insert(&pool, common::sample_remote_actor("bob", "remote.test"))
        .await
        .unwrap();
    let id = insert_note(
        &pool,
        actor.id,
        "remote.test",
        "n1",
        "remote content",
        false,
    )
    .await;

    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::routes::router(state);

    let resp = app
        .oneshot(
            Request::get(format!("/notes/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn permalink_returns_ap_json_when_accept_activitystreams(pool: PgPool) {
    let actor = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let id = insert_note(&pool, actor.id, "example.test", "n1", "hello world", true).await;

    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::routes::router(state);

    let resp = app
        .clone()
        .oneshot(
            Request::get(format!("/notes/{id}"))
                .header(header::ACCEPT, "application/activity+json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp.headers().get(header::CONTENT_TYPE).unwrap();
    assert_eq!(ct, "application/activity+json");

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["type"], "Note");
    assert_eq!(json["content"], "hello world");
    assert_eq!(json["attributedTo"], "https://example.test/users/alice");
    assert_eq!(
        json["id"],
        format!("https://example.test/notes/n1"),
        "AP id must equal stored ap_id: {json}",
    );
    // ld+json (profile 付き) でも JSON が返る。
    let resp2 = app
        .oneshot(
            Request::get(format!("/notes/{id}"))
                .header(
                    header::ACCEPT,
                    r#"application/ld+json; profile="https://www.w3.org/ns/activitystreams""#,
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);
    assert_eq!(
        resp2.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/activity+json"
    );
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn permalink_renders_local_note_with_escaped_content(pool: PgPool) {
    let actor = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    // 攻撃者の content を想定: `<script>` が escape されずに出ると XSS。
    let id = insert_note(
        &pool,
        actor.id,
        "example.test",
        "n1",
        "<script>alert('x')</script>",
        true,
    )
    .await;

    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::routes::router(state);

    let resp = app
        .oneshot(
            Request::get(format!("/notes/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp.headers().get(header::CONTENT_TYPE).unwrap();
    assert_eq!(ct, "text/html; charset=utf-8");

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let html = std::str::from_utf8(&body).unwrap();

    // content は HTML escape されている (M4 PR1 はサニタイザ未実装)。
    assert!(
        html.contains("&lt;script&gt;alert(&#x27;x&#x27;)&lt;/script&gt;"),
        "escaped content not found: {html}"
    );
    // 生 `<script>` が出ていないこと (XSS regression テスト)。
    assert!(
        !html.contains("<script>alert"),
        "raw <script> leaked: {html}"
    );
    // actor へのリンクが入っていること。
    assert!(
        html.contains(r#"<a href="/users/alice""#),
        "actor link missing: {html}"
    );
}

/// permalink AP JSON は `attachment` フィールドを Note `Document` 配列として
/// 出す ── 元の `Create` activity と同じレイアウトを返すことで、リモートが
/// canonical URL から refetch しても添付が落ちないようにする。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn permalink_ap_json_includes_attachment(pool: PgPool) {
    let actor = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let note_id = insert_note(
        &pool,
        actor.id,
        "example.test",
        "with-attach",
        "look at this",
        true,
    )
    .await;
    // 添付 media を 2 件 insert → attach_to_note。`list_by_note` は id ASC で
    // 返るので a, b の順で並ぶ。
    let m_a = repo::media::insert(
        &pool,
        repo::media::NewMedia {
            storage_key: "att-a.webp".into(),
            media_type: "image/webp".into(),
            width: 800,
            height: 600,
            byte_size: 1234,
            kind: "attachment".into(),
            alt_text: Some("first".into()),
            owner_actor_id: actor.id,
        },
    )
    .await
    .unwrap();
    let m_b = repo::media::insert(
        &pool,
        repo::media::NewMedia {
            storage_key: "att-b.webp".into(),
            media_type: "image/webp".into(),
            width: 400,
            height: 400,
            byte_size: 5678,
            kind: "attachment".into(),
            alt_text: None,
            owner_actor_id: actor.id,
        },
    )
    .await
    .unwrap();
    repo::media::attach_to_note(&pool, &[m_a.id, m_b.id], actor.id, note_id)
        .await
        .unwrap();

    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::routes::router(state);

    let resp = app
        .oneshot(
            Request::get(format!("/notes/{note_id}"))
                .header(header::ACCEPT, "application/activity+json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let attachments = json["attachment"]
        .as_array()
        .expect("attachment field must be present and array");
    assert_eq!(attachments.len(), 2, "expected 2 attachments: {json}");
    assert_eq!(attachments[0]["type"], "Document");
    assert_eq!(attachments[0]["mediaType"], "image/webp");
    assert_eq!(
        attachments[0]["url"],
        "https://example.test/media/att-a.webp"
    );
    assert_eq!(attachments[0]["width"], 800);
    assert_eq!(attachments[0]["height"], 600);
    // alt_text あり → name フィールドあり
    assert_eq!(attachments[0]["name"], "first");
    // alt_text 無し → name 欠落 (= AS2 として valid)
    assert!(
        attachments[1].get("name").is_none(),
        "no-alt attachment must not carry 'name': {:?}",
        attachments[1]
    );
    assert_eq!(
        attachments[1]["url"],
        "https://example.test/media/att-b.webp"
    );
}

/// 添付が無い Note は `attachment` フィールドを **持たない** (= 空配列でなく
/// 欠落)。AS2 の慣習に合わせる ── 空配列を出すパーサ実装もあれば、欠落で
/// 表現する実装もあるが、元の `Create` activity も `attachments.is_empty()`
/// 時はフィールド自体を出さないのでそれに合わせる。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn permalink_ap_json_omits_attachment_when_none(pool: PgPool) {
    let actor = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let id = insert_note(&pool, actor.id, "example.test", "n1", "no media", true).await;
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::routes::router(state);
    let resp = app
        .oneshot(
            Request::get(format!("/notes/{id}"))
                .header(header::ACCEPT, "application/activity+json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json.get("attachment").is_none(),
        "attachment field must be absent for empty: {json}"
    );
}

/// **SECURITY (緊急 fix)**: `followers` 可視性の note は URL 直アクセスで漏れない。
/// permalink は unauthenticated な公開 endpoint なので、AS2 audience に Public が
/// 含まれない note は 404 で返して存在自体を秘匿する (Mastodon 同様)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn permalink_404_for_followers_only_note(pool: PgPool) {
    let actor = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let id = insert_note_with_visibility(
        &pool,
        actor.id,
        "example.test",
        "1",
        "followers-only secret",
        true,
        Visibility::Followers,
    )
    .await;

    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::routes::router(state);

    // HTML 経路
    let resp = app
        .clone()
        .oneshot(
            Request::get(format!("/notes/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // AP JSON 経路でも漏れないこと (= followers note の to/cc が公開 fetch で取れない)
    let resp = app
        .oneshot(
            Request::get(format!("/notes/{id}"))
                .header(header::ACCEPT, "application/activity+json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// 同じく `direct` (DM) 可視性も 404 で隠す。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn permalink_404_for_direct_note(pool: PgPool) {
    let actor = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let id = insert_note_with_visibility(
        &pool,
        actor.id,
        "example.test",
        "1",
        "direct dm secret",
        true,
        Visibility::Direct,
    )
    .await;

    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::routes::router(state);

    // HTML 経路
    let resp = app
        .clone()
        .oneshot(
            Request::get(format!("/notes/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // AP JSON 経路でも漏れないこと (= direct note の to/cc が公開 fetch で取れない)
    let resp = app
        .oneshot(
            Request::get(format!("/notes/{id}"))
                .header(header::ACCEPT, "application/activity+json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// 対照: `unlisted` は public timeline には載らないが、permalink は公開 (= URL
/// を知っている人 / 連合相手 fetch が見られる慣習。Mastodon と同じ)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn permalink_200_for_unlisted_note(pool: PgPool) {
    let actor = repo::actor::insert(&pool, common::sample_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let id = insert_note_with_visibility(
        &pool,
        actor.id,
        "example.test",
        "1",
        "unlisted message",
        true,
        Visibility::Unlisted,
    )
    .await;

    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::routes::router(state);

    let resp = app
        .oneshot(
            Request::get(format!("/notes/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
