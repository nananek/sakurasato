//! M4 PR1 統合テスト: `GET /media/{*key}` パストラバーサル防御。
//!
//! S3 が実機に居ないテスト環境では正常系 (versitygw に PUT して GET で取る)
//! は走らせられないので、本ファイルでは **handler 自身による key 検証** が
//! S3 client 呼び出し **前** に効くことだけ確認する。
//! - `/media/../secret` → 400 (S3 へ到達する前に弾く)
//! - `/media/a%2F..%2Fb` → axum が `a/../b` にデコードし 400
//! - `/media/` → そもそも `{*key}` がマッチしないので 404
//!
//! 正常系 (200 + body) は compose 統合テスト (今後の milestone) で網羅する。

#![forbid(unsafe_code)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use sakurasato_core::model::Visibility;
use sakurasato_core::repo;
use sakurasato_core::repo::actor::NewActor;
use sakurasato_core::repo::media::NewMedia;
use sakurasato_core::repo::note::NewNote;
use sqlx::PgPool;
use tower::ServiceExt;

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
            video: sakurasato_core::config::VideoConfig::default(),
            emoji_import: sakurasato_core::config::EmojiImportConfig::default(),
        },
        miauth: None,
    }
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn media_rejects_dotdot_traversal(pool: PgPool) {
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::routes::router(state);

    let resp = app
        .oneshot(
            Request::get("/media/a/../../secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // is_safe_key が S3 呼び出しの前に弾くので 400。S3 まで届いていれば
    // 接続失敗で 500 になるはずなので、400 が返ることで「key 検証が手前で
    // 動いている」ことが確認できる。400 にも no-store が付く (#261)。
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        resp.headers()
            .get(axum::http::header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("no-store"),
    );
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn media_rejects_url_encoded_traversal(pool: PgPool) {
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::routes::router(state);

    // `%2F` は `/`、`%2E%2E` は `..`。axum が percent-decode してから path
    // 抽出するため、is_safe_key の `..` セグメント検出にかかる。
    let resp = app
        .oneshot(
            Request::get("/media/a%2F%2E%2E%2Fb")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn media_rejects_double_slash(pool: PgPool) {
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::routes::router(state);

    let resp = app
        .oneshot(Request::get("/media/a//b.png").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ── authorization layer のテスト ─────────────────────────────────────
//
// 本テスト群は S3 が居ない環境を前提に、authorization が通過すると S3 接続
// 失敗で **500** に倒れ、authorization が拒否すると **404** に倒れる差で
// 「authorization 層が効いている」ことを確認する。S3 まで届かない (= 早期
// reject) は 404、S3 まで届いた (= 許可) は 500 という対比。
//
// 通過 (S3 fetch まで進む) ケース:
//   - `kind = avatar` (= 常に public)
//   - `kind = attachment` (note 紐付け状態 / visibility 不問)
//   - `emoji/local/...` prefix
// 拒否 (= 404) ケース:
//   - 未知 key (= media table に row 無し)
//
// `attachment` の visibility ガード (`followers` / `direct` → 404) は
// PR #108 で導入したが、Fediverse 標準は media URL を *URL obscurity* で
// 防衛する慣行 (= Mastodon / Misskey も非認証で配信) で、private 投稿の
// 画像がリモートで壊れて見える致命的副作用があったため撤回した。
// `note_id IS NULL` (= 未添付) の孤児ガードも #261 で撤去 ── Misskey drive
// モデルではアップロード直後から閲覧可能で、404 が CDN に負キャッシュされ
// 「投稿後も画像が 404」になっていた。詳細は
// `crates/server/src/routes/media.rs::authorized_for_public` の doc を見る。
//
// 負レスポンス (400 / 404 / 500) には `Cache-Control: no-store` が付くこと
// (#261) も本テスト群で固定する (400 = traversal、404 = 未知 key、500 =
// reaches_s3 の S3 接続失敗ブランチ)。

fn seed_local_actor(username: &str, host: &str) -> NewActor {
    let ap_id = format!("https://{host}/users/{username}");
    NewActor {
        ap_id: ap_id.clone(),
        preferred_username: username.into(),
        host: host.into(),
        display_name: None,
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

fn new_media(storage_key: &str, kind: &str, owner_actor_id: i64) -> NewMedia {
    NewMedia {
        storage_key: storage_key.into(),
        media_type: "image/webp".into(),
        width: 256,
        height: 256,
        byte_size: 1024,
        kind: kind.into(),
        alt_text: None,
        owner_actor_id,
        duration_ms: None,
    }
}

fn new_note(actor_id: i64, ap_suffix: &str, visibility: Visibility) -> NewNote {
    NewNote {
        ap_id: format!("https://example.test/notes/{ap_suffix}"),
        actor_id,
        content: "test".into(),
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
        is_local: true,
        url: None,
        published_at: chrono::Utc::now(),
    }
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn media_unknown_key_returns_404_no_store(pool: PgPool) {
    // media table に行が無い key は authorization で拒否 → 404。
    // 404 には `Cache-Control: no-store` が付く (#261) ── `.webp` は CDN の
    // デフォルトキャッシュ対象拡張子で、負キャッシュが残ると後から正常化
    // しても 404 に見え続けるため。
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::routes::router(state);

    let resp = app
        .oneshot(
            Request::get("/media/nonexistent.webp")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        resp.headers()
            .get(axum::http::header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("no-store"),
    );
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn media_orphan_attachment_reaches_s3(pool: PgPool) {
    // kind=attachment + note_id NULL (= drive アップロード直後、note 未紐付)
    // も配信する (#261)。Misskey drive モデルではアップロード直後から
    // `DriveFile.url` が閲覧可能で、ここで 404 を返すと CDN の負キャッシュが
    // 「投稿後も画像が 404」を固定化していた。authorization 通過 → S3 不在
    // 環境では 500 に倒れる (= 本 suite の reaches_s3 規約)。
    let actor = repo::actor::insert(&pool, seed_local_actor("alice", "example.test"))
        .await
        .unwrap();
    repo::media::insert(&pool, new_media("orphan.webp", "attachment", actor.id))
        .await
        .unwrap();

    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::routes::router(state);
    let resp = app
        .oneshot(
            Request::get("/media/orphan.webp")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    // 5xx も no-store (#261) ── S3 障害中の一時的な 500 が CDN に焼き付いて
    // 復旧後も「壊れた画像」に見え続けないように。
    assert_eq!(
        resp.headers()
            .get(axum::http::header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("no-store"),
    );
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn media_attachment_on_followers_note_reaches_s3(pool: PgPool) {
    // followers-only note に紐付いた attachment は AP 配送で audience に
    // URL が渡っており、Mastodon の media proxy は post-delivery で URL を
    // 非認証 GET する。ここで 404 を返すと「リモートで画像が壊れる」状態
    // (= 元 PR #108 が踏んだ過剰補正) なので、Fediverse 標準どおり
    // visibility に関わらず通す。S3 が居ないテスト環境では fetch が失敗し
    // て 500 まで進むことで「authorization 通過」を確認する。
    let actor = repo::actor::insert(&pool, seed_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let note = repo::note::insert(&pool, new_note(actor.id, "1", Visibility::Followers))
        .await
        .unwrap();
    let media = repo::media::insert(&pool, new_media("priv.webp", "attachment", actor.id))
        .await
        .unwrap();
    repo::media::attach_to_note(&pool, &[media.id], actor.id, note.id)
        .await
        .unwrap();

    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::routes::router(state);
    let resp = app
        .oneshot(
            Request::get("/media/priv.webp")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn media_attachment_on_direct_note_reaches_s3(pool: PgPool) {
    // direct (DM) note の attachment も followers と同じ理由で通す
    // (= `media_attachment_on_followers_note_reaches_s3` 参照)。
    let actor = repo::actor::insert(&pool, seed_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let note = repo::note::insert(&pool, new_note(actor.id, "2", Visibility::Direct))
        .await
        .unwrap();
    let media = repo::media::insert(&pool, new_media("dm.webp", "attachment", actor.id))
        .await
        .unwrap();
    repo::media::attach_to_note(&pool, &[media.id], actor.id, note.id)
        .await
        .unwrap();

    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::routes::router(state);
    let resp = app
        .oneshot(Request::get("/media/dm.webp").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn media_attachment_on_public_note_reaches_s3(pool: PgPool) {
    // public note 紐付け attachment は authorization 通過 → S3 fetch で 500。
    let actor = repo::actor::insert(&pool, seed_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let note = repo::note::insert(&pool, new_note(actor.id, "3", Visibility::Public))
        .await
        .unwrap();
    let media = repo::media::insert(&pool, new_media("pub.webp", "attachment", actor.id))
        .await
        .unwrap();
    repo::media::attach_to_note(&pool, &[media.id], actor.id, note.id)
        .await
        .unwrap();

    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::routes::router(state);
    let resp = app
        .oneshot(Request::get("/media/pub.webp").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn media_public_redirect_302_for_authorized_key(pool: PgPool) {
    // `public_base_url` 設定時、認可済 key は S3 を叩かず 302 で公開 base へ。
    let actor = repo::actor::insert(&pool, seed_local_actor("alice", "example.test"))
        .await
        .unwrap();
    let note = repo::note::insert(&pool, new_note(actor.id, "redir", Visibility::Public))
        .await
        .unwrap();
    let media = repo::media::insert(&pool, new_media("pub.webp", "attachment", actor.id))
        .await
        .unwrap();
    repo::media::attach_to_note(&pool, &[media.id], actor.id, note.id)
        .await
        .unwrap();

    // 末尾スラッシュ付き base を渡し、正規化 (double-slash 回避) も検証する。
    let mut cfg = make_config("example.test");
    cfg.storage.public_base_url = Some("https://media.example.test/".into());
    let state = sakurasato_server::state::AppState::from_pool(pool, cfg);
    let app = sakurasato_server::routes::router(state);
    let resp = app
        .oneshot(Request::get("/media/pub.webp").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FOUND);
    assert_eq!(
        resp.headers()
            .get(axum::http::header::LOCATION)
            .and_then(|v| v.to_str().ok()),
        Some("https://media.example.test/pub.webp"),
    );
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn media_public_redirect_302_for_orphan(pool: PgPool) {
    // `public_base_url` 設定時も、orphan attachment (note 未紐付) は #261 で
    // 配信対象 → 302 リダイレクト。drive アップロード直後のプレビュー GET が
    // このパスに乗る (R2 には upload 時点で PUT 済みなので 302 先は実在する)。
    let actor = repo::actor::insert(&pool, seed_local_actor("alice", "example.test"))
        .await
        .unwrap();
    repo::media::insert(&pool, new_media("orphan.webp", "attachment", actor.id))
        .await
        .unwrap();

    let mut cfg = make_config("example.test");
    cfg.storage.public_base_url = Some("https://media.example.test".into());
    let state = sakurasato_server::state::AppState::from_pool(pool, cfg);
    let app = sakurasato_server::routes::router(state);
    let resp = app
        .oneshot(
            Request::get("/media/orphan.webp")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FOUND);
    assert_eq!(
        resp.headers()
            .get(axum::http::header::LOCATION)
            .and_then(|v| v.to_str().ok()),
        Some("https://media.example.test/orphan.webp"),
    );
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn media_avatar_reaches_s3(pool: PgPool) {
    // kind=avatar は actor の icon として常に public → 通過、S3 で 500。
    let actor = repo::actor::insert(&pool, seed_local_actor("alice", "example.test"))
        .await
        .unwrap();
    repo::media::insert(&pool, new_media("av.webp", "avatar", actor.id))
        .await
        .unwrap();

    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::routes::router(state);
    let resp = app
        .oneshot(Request::get("/media/av.webp").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn media_emoji_prefix_reaches_s3(pool: PgPool) {
    // emoji/local/<shortcode>.webp は media table を経由せず prefix で許可。
    let state = sakurasato_server::state::AppState::from_pool(pool, make_config("example.test"));
    let app = sakurasato_server::routes::router(state);

    let resp = app
        .oneshot(
            Request::get("/media/emoji/local/foo.webp")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}
