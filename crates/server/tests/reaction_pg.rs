//! M8 PR2 統合テスト: `Like` / `EmojiReact` / `Undo` 受領経路。
//!
//! `dispatch_pg.rs` の Follow パターンを参考に、署名検証 → F3 → handler 経由で
//! `reaction` 行が入る (または既存行が消える) 経路を E2E で通す。

#![forbid(unsafe_code)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use http_body_util::BodyExt;
use rsa::RsaPrivateKey;
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey, EncodePublicKey, LineEnding};
use rsa::rand_core::OsRng;
use rsa::signature::{SignatureEncoding, SignerMut};
use sakurasato_core::model::Visibility;
use sakurasato_core::repo;
use sakurasato_core::repo::actor::NewActor;
use sakurasato_server::routes::router;
use sakurasato_server::state::AppState;
use sqlx::PgPool;
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

fn remote_actor(host: &str, user: &str, pub_pem: &str) -> NewActor {
    let ap_id = format!("https://{host}/users/{user}");
    NewActor {
        ap_id: ap_id.clone(),
        preferred_username: user.into(),
        host: host.into(),
        display_name: None,
        summary: None,
        icon_url: None,
        image_url: None,
        inbox_url: format!("{ap_id}/inbox"),
        shared_inbox_url: Some(format!("https://{host}/inbox")),
        outbox_url: Some(format!("{ap_id}/outbox")),
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

async fn seed_local_note(pool: &PgPool, local_actor_id: i64) -> (i64, String) {
    let ap_id = format!("https://{LOCAL_HOST}/notes/1");
    let inserted = repo::note::insert(
        pool,
        repo::note::NewNote {
            ap_id: ap_id.clone(),
            actor_id: local_actor_id,
            content: "hello federation".into(),
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
            url: Some(ap_id.clone()),
            published_at: chrono::Utc::now(),
        },
    )
    .await
    .unwrap();
    (inserted.id, ap_id)
}

/// `seed_local_note` の remote 版。`author_actor_id` の remote actor が書いた
/// `is_local: false` の note を 1 件保持する ── 実運用では followee 投稿 /
/// フォロイーの boost 経由で取り込まれ「タイムラインに並ぶ」note を模す。
async fn seed_remote_note(
    pool: &PgPool,
    author_actor_id: i64,
    host: &str,
    n: u32,
) -> (i64, String) {
    let ap_id = format!("https://{host}/notes/{n}");
    let inserted = repo::note::insert(
        pool,
        repo::note::NewNote {
            ap_id: ap_id.clone(),
            actor_id: author_actor_id,
            content: "remote timeline post".into(),
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
            is_local: false,
            url: Some(ap_id.clone()),
            published_at: chrono::Utc::now(),
        },
    )
    .await
    .unwrap();
    (inserted.id, ap_id)
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn inbound_like_records_reaction(pool: PgPool) {
    let (_, local_pub) = fresh_rsa();
    let (remote_priv, remote_pub) = fresh_rsa();
    let local = repo::actor::insert(&pool, local_actor(&local_pub, "PRIV"))
        .await
        .unwrap();
    let remote = repo::actor::insert(&pool, remote_actor("remote.test", "bob", &remote_pub))
        .await
        .unwrap();
    let (note_id, note_ap_id) = seed_local_note(&pool, local.id).await;

    let state = AppState::from_pool(pool.clone(), make_config());
    let app = router(state);

    let activity_id = "https://remote.test/users/bob/activities/like-1".to_string();
    let body = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": activity_id,
        "type": "Like",
        "actor": remote.ap_id,
        "object": note_ap_id,
    })
    .to_string();
    let keyid = format!("{}#main-key", remote.ap_id);
    let req = build_signed_post(body.as_bytes(), "/inbox", &remote_priv, &keyid, LOCAL_HOST);

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let row = repo::reaction::get_by_ap_id(&pool, &activity_id)
        .await
        .unwrap()
        .expect("reaction row should exist");
    assert_eq!(row.note_id, note_id);
    assert_eq!(row.actor_id, remote.id);
    // content 無し Like は空文字。
    assert_eq!(row.content, "");
    assert!(row.emoji_id.is_none());

    // **in-app 通知** (migration 0020) も 1 件立つ ── dispatch handler が
    // `notify(Reaction)` を呼び、webhook と並行して notification 行を insert する。
    let notifs = repo::notification::list(&pool, local.id, 10, None, None)
        .await
        .unwrap();
    assert_eq!(notifs.len(), 1, "reaction で in-app 通知が 1 件立つ");
    assert_eq!(notifs[0].event_type, "reaction");
    assert_eq!(notifs[0].notifier_actor_id, Some(remote.id));
    assert_eq!(notifs[0].note_id, Some(note_id));
    assert!(!notifs[0].is_read, "新規通知は未読");
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn inbound_emoji_react_learns_remote_emoji(pool: PgPool) {
    let (_, local_pub) = fresh_rsa();
    let (remote_priv, remote_pub) = fresh_rsa();
    let local = repo::actor::insert(&pool, local_actor(&local_pub, "PRIV"))
        .await
        .unwrap();
    let remote = repo::actor::insert(&pool, remote_actor("remote.test", "bob", &remote_pub))
        .await
        .unwrap();
    let (_, note_ap_id) = seed_local_note(&pool, local.id).await;

    let state = AppState::from_pool(pool.clone(), make_config());
    let app = router(state);

    let emoji_ap_id = "https://remote.test/emojis/blob_party";
    let activity_id = "https://remote.test/users/bob/activities/react-1".to_string();
    let body = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": activity_id,
        "type": "EmojiReact",
        "actor": remote.ap_id,
        "object": note_ap_id,
        "content": ":blob_party:",
        "tag": [{
            "type": "Emoji",
            "id": emoji_ap_id,
            "name": ":blob_party:",
            "icon": {
                "type": "Image",
                "mediaType": "image/png",
                "url": "https://remote.test/files/blob.png"
            }
        }]
    })
    .to_string();
    let keyid = format!("{}#main-key", remote.ap_id);
    let req = build_signed_post(body.as_bytes(), "/inbox", &remote_priv, &keyid, LOCAL_HOST);

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // remote emoji が学習された。
    let emoji = repo::emoji::get_by_ap_id(&pool, emoji_ap_id)
        .await
        .unwrap()
        .expect("remote emoji should be learned");
    assert_eq!(emoji.shortcode, "blob_party");
    assert_eq!(emoji.host.as_deref(), Some("remote.test"));
    assert!(!emoji.is_local);

    // reaction 行が学習した emoji_id を参照している。Issue #242: remote 絵文字を
    // 学習できたので content は host を保った `:blob_party@remote.test:` 形になる
    // (host 無し `:blob_party:` を渡すと Aria が自鯖ローカル絵文字と誤認するため)。
    let row = repo::reaction::get_by_ap_id(&pool, &activity_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.content, ":blob_party@remote.test:");
    assert_eq!(row.emoji_id, Some(emoji.id));
}

/// Issue #239: signer = `remote.test`, `Emoji.id` = `remote.test` (一致) だが、
/// `icon.url` は `drive-remote.test` (別ドメイン drive)。Misskey の典型構成で、
/// `icon.url` host 検査を撤廃したので emoji は学習される。
///
/// 注: テスト環境の media-proxy は dead socket (`make_config` の `/tmp/x`) なので
/// 画像 fetch は失敗し `image_key=None` + `last_failed_at=Some` になるが、emoji ROW と
/// `reaction.emoji_id` は書かれる (fetch 失敗は upsert を妨げない)。
#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn inbound_emoji_react_learns_emoji_with_separate_drive_host(pool: PgPool) {
    let (_, local_pub) = fresh_rsa();
    let (remote_priv, remote_pub) = fresh_rsa();
    let local = repo::actor::insert(&pool, local_actor(&local_pub, "PRIV"))
        .await
        .unwrap();
    let remote = repo::actor::insert(&pool, remote_actor("remote.test", "bob", &remote_pub))
        .await
        .unwrap();
    let (_, note_ap_id) = seed_local_note(&pool, local.id).await;

    let state = AppState::from_pool(pool.clone(), make_config());
    let app = router(state);

    let emoji_ap_id = "https://remote.test/emojis/blobcat";
    let activity_id = "https://remote.test/users/bob/activities/react-drive".to_string();
    let body = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": activity_id,
        "type": "EmojiReact",
        "actor": remote.ap_id,
        "object": note_ap_id,
        "content": ":blobcat:",
        "tag": [{
            "type": "Emoji",
            "id": emoji_ap_id,
            "name": ":blobcat:",
            "icon": {
                "type": "Image",
                "mediaType": "image/webp",
                // 別ドメイン drive。Emoji.id (remote.test) とは host が異なる。
                "url": "https://drive-remote.test/files/blobcat.webp"
            }
        }]
    })
    .to_string();
    let keyid = format!("{}#main-key", remote.ap_id);
    let req = build_signed_post(body.as_bytes(), "/inbox", &remote_priv, &keyid, LOCAL_HOST);

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // 別ドメイン drive の emoji でも学習される (= #239 の本丸)。
    let emoji = repo::emoji::get_by_ap_id(&pool, emoji_ap_id)
        .await
        .unwrap()
        .expect("separate-drive-host emoji should be learned");
    assert_eq!(emoji.shortcode, "blobcat");
    assert_eq!(emoji.host.as_deref(), Some("remote.test")); // host は signer 由来
    assert!(!emoji.is_local);
    // dead-socket media-proxy のため画像は焼けていない (fetch 失敗 backoff)。
    assert!(emoji.image_key.is_none());
    assert!(emoji.last_failed_at.is_some());

    // reaction 行が学習した emoji_id を参照している。Issue #242: 画像 fetch が
    // 失敗 (dead socket) しても emoji は学習済みなので、content は host を保った
    // `:blobcat@remote.test:` 形になる (host は signer 由来)。
    let row = repo::reaction::get_by_ap_id(&pool, &activity_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.content, ":blobcat@remote.test:");
    assert_eq!(row.emoji_id, Some(emoji.id));
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn inbound_emoji_react_rejects_cross_host_emoji_tag(pool: PgPool) {
    // signer は remote.test だが、tag の Emoji.id は other.test を指す ──
    // 他インスタンスの emoji ID を spoofing 学習しないこと。reaction は記録
    // されるが emoji_id は None。
    // Issue #239: 拒否は **Emoji.id** host 不一致による (icon.url の host 検査は
    // 撤廃済み)。icon.url が other.test でも、reject は Emoji.id が担う。
    let (_, local_pub) = fresh_rsa();
    let (remote_priv, remote_pub) = fresh_rsa();
    let local = repo::actor::insert(&pool, local_actor(&local_pub, "PRIV"))
        .await
        .unwrap();
    let remote = repo::actor::insert(&pool, remote_actor("remote.test", "bob", &remote_pub))
        .await
        .unwrap();
    let (_, note_ap_id) = seed_local_note(&pool, local.id).await;

    let state = AppState::from_pool(pool.clone(), make_config());
    let app = router(state);

    let spoofed_emoji = "https://other.test/emojis/blob";
    let activity_id = "https://remote.test/users/bob/activities/react-spoof".to_string();
    let body = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": activity_id,
        "type": "EmojiReact",
        "actor": remote.ap_id,
        "object": note_ap_id,
        "content": ":blob:",
        "tag": [{
            "type": "Emoji",
            "id": spoofed_emoji,
            "name": ":blob:",
            "icon": {"type": "Image", "mediaType": "image/png", "url": "https://other.test/files/blob.png"}
        }]
    })
    .to_string();
    let keyid = format!("{}#main-key", remote.ap_id);
    let req = build_signed_post(body.as_bytes(), "/inbox", &remote_priv, &keyid, LOCAL_HOST);

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // spoof 元 emoji は学習されない。
    let learned = repo::emoji::get_by_ap_id(&pool, spoofed_emoji)
        .await
        .unwrap();
    assert!(learned.is_none(), "cross-host emoji must not be learned");

    // reaction 自体は入る (content だけ持つ)。
    let row = repo::reaction::get_by_ap_id(&pool, &activity_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.content, ":blob:");
    assert!(row.emoji_id.is_none());
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn inbound_undo_deletes_existing_reaction(pool: PgPool) {
    let (_, local_pub) = fresh_rsa();
    let (remote_priv, remote_pub) = fresh_rsa();
    let local = repo::actor::insert(&pool, local_actor(&local_pub, "PRIV"))
        .await
        .unwrap();
    let remote = repo::actor::insert(&pool, remote_actor("remote.test", "bob", &remote_pub))
        .await
        .unwrap();
    let (note_id, _) = seed_local_note(&pool, local.id).await;

    let reaction_ap_id = "https://remote.test/users/bob/activities/like-2";
    repo::reaction::insert(&pool, reaction_ap_id, note_id, remote.id, "👍", None)
        .await
        .unwrap();

    let state = AppState::from_pool(pool.clone(), make_config());
    let app = router(state);

    let undo_id = "https://remote.test/users/bob/activities/undo-1";
    let body = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": undo_id,
        "type": "Undo",
        "actor": remote.ap_id,
        "object": reaction_ap_id,
    })
    .to_string();
    let keyid = format!("{}#main-key", remote.ap_id);
    let req = build_signed_post(body.as_bytes(), "/inbox", &remote_priv, &keyid, LOCAL_HOST);

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let gone = repo::reaction::get_by_ap_id(&pool, reaction_ap_id)
        .await
        .unwrap();
    assert!(gone.is_none(), "reaction must be deleted by Undo");
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn inbound_undo_refuses_when_signer_is_not_reactor(pool: PgPool) {
    // Eve が Bob のリアクションを Undo しようとする ── 弾く (Malformed → 400)。
    let (_, local_pub) = fresh_rsa();
    let (_, bob_pub) = fresh_rsa();
    let (eve_priv, eve_pub) = fresh_rsa();
    let local = repo::actor::insert(&pool, local_actor(&local_pub, "PRIV"))
        .await
        .unwrap();
    let bob = repo::actor::insert(&pool, remote_actor("remote.test", "bob", &bob_pub))
        .await
        .unwrap();
    let eve = repo::actor::insert(&pool, remote_actor("evil.test", "eve", &eve_pub))
        .await
        .unwrap();
    let (note_id, _) = seed_local_note(&pool, local.id).await;

    let reaction_ap_id = "https://remote.test/users/bob/activities/like-3";
    repo::reaction::insert(&pool, reaction_ap_id, note_id, bob.id, "👍", None)
        .await
        .unwrap();

    let state = AppState::from_pool(pool.clone(), make_config());
    let app = router(state);

    let undo_id = "https://evil.test/users/eve/activities/undo-foul";
    let body = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": undo_id,
        "type": "Undo",
        "actor": eve.ap_id,
        "object": reaction_ap_id,
    })
    .to_string();
    let keyid = format!("{}#main-key", eve.ap_id);
    let req = build_signed_post(body.as_bytes(), "/inbox", &eve_priv, &keyid, LOCAL_HOST);

    let resp = app.oneshot(req).await.unwrap();
    // Undo signer != reaction actor → Malformed (400)。
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // 残っていること。
    let still_there = repo::reaction::get_by_ap_id(&pool, reaction_ap_id)
        .await
        .unwrap();
    assert!(still_there.is_some(), "reaction must not be deleted");
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn inbound_emoji_react_without_content_returns_400(pool: PgPool) {
    let (_, local_pub) = fresh_rsa();
    let (remote_priv, remote_pub) = fresh_rsa();
    let local = repo::actor::insert(&pool, local_actor(&local_pub, "PRIV"))
        .await
        .unwrap();
    let remote = repo::actor::insert(&pool, remote_actor("remote.test", "bob", &remote_pub))
        .await
        .unwrap();
    let (_, note_ap_id) = seed_local_note(&pool, local.id).await;

    let state = AppState::from_pool(pool.clone(), make_config());
    let app = router(state);

    let activity_id = "https://remote.test/users/bob/activities/react-empty".to_string();
    let body = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": activity_id,
        "type": "EmojiReact",
        "actor": remote.ap_id,
        "object": note_ap_id,
    })
    .to_string();
    let keyid = format!("{}#main-key", remote.ap_id);
    let req = build_signed_post(body.as_bytes(), "/inbox", &remote_priv, &keyid, LOCAL_HOST);

    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    // body は読み捨て (assertion メッセージで内容を見たいときだけ展開)。
    let _ = resp.into_body().collect().await.unwrap();
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn inbound_like_to_unknown_note_is_silently_accepted(pool: PgPool) {
    // 我々が知らない Note URI に対する Like → 202 で受け流し、DB は触らない。
    let (_, local_pub) = fresh_rsa();
    let (remote_priv, remote_pub) = fresh_rsa();
    let _local = repo::actor::insert(&pool, local_actor(&local_pub, "PRIV"))
        .await
        .unwrap();
    let remote = repo::actor::insert(&pool, remote_actor("remote.test", "bob", &remote_pub))
        .await
        .unwrap();

    let state = AppState::from_pool(pool.clone(), make_config());
    let app = router(state);

    let activity_id = "https://remote.test/users/bob/activities/like-unknown".to_string();
    let body = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": activity_id,
        "type": "Like",
        "actor": remote.ap_id,
        "object": "https://sakura.test/notes/does-not-exist",
    })
    .to_string();
    let keyid = format!("{}#main-key", remote.ap_id);
    let req = build_signed_post(body.as_bytes(), "/inbox", &remote_priv, &keyid, LOCAL_HOST);

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let gone = repo::reaction::get_by_ap_id(&pool, &activity_id)
        .await
        .unwrap();
    assert!(gone.is_none(), "no reaction row should be created");
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn inbound_reaction_on_remote_note_is_recorded(pool: PgPool) {
    // フォロイー (carol@remote.test) の投稿 = タイムラインに並ぶ remote note に、
    // **別の** remote user (bob@other.test) がリアクション → カウント反映する
    // (Misskey 準拠)。ただし our own note ではないので in-app 通知は出さない。
    // dispatch は「note が DB にあること」だけを見る (local/remote 問わず)。
    let (_, local_pub) = fresh_rsa();
    let (_, carol_pub) = fresh_rsa();
    let (bob_priv, bob_pub) = fresh_rsa();
    let local = repo::actor::insert(&pool, local_actor(&local_pub, "PRIV"))
        .await
        .unwrap();
    let carol = repo::actor::insert(&pool, remote_actor("remote.test", "carol", &carol_pub))
        .await
        .unwrap();
    let bob = repo::actor::insert(&pool, remote_actor("other.test", "bob", &bob_pub))
        .await
        .unwrap();
    let (note_id, note_ap_id) = seed_remote_note(&pool, carol.id, "remote.test", 42).await;

    let state = AppState::from_pool(pool.clone(), make_config());
    let app = router(state);

    let activity_id = "https://other.test/users/bob/activities/like-remote".to_string();
    let body = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": activity_id,
        "type": "Like",
        "actor": bob.ap_id,
        "object": note_ap_id,
        "content": "👍",
    })
    .to_string();
    let keyid = format!("{}#main-key", bob.ap_id);
    let req = build_signed_post(body.as_bytes(), "/inbox", &bob_priv, &keyid, LOCAL_HOST);

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // reaction が remote note に記録される (= 従来は is_local ガードで捨てていた)。
    let row = repo::reaction::get_by_ap_id(&pool, &activity_id)
        .await
        .unwrap()
        .expect("reaction on a stored remote note should be recorded");
    assert_eq!(row.note_id, note_id);
    assert_eq!(row.actor_id, bob.id);
    assert_eq!(row.content, "👍");

    // our own note ではないので in-app 通知は立たない (カウントのみ反映)。
    let notifs = repo::notification::list(&pool, local.id, 10, None, None)
        .await
        .unwrap();
    assert!(
        notifs.is_empty(),
        "remote note への第三者 reaction では通知を出さない (got {} notifs)",
        notifs.len(),
    );
}
