//! `emoji_backfill::run_with_state` (CLI `emoji backfill-remote` の本体) の
//! 統合テスト。
//!
//! - `is_local=false` かつ `tag: [Emoji]` を含む蓄積済み Note からリモート
//!   絵文字が学習されること。
//! - `is_local=true` の Note やタグなし Note は対象外であること。
//! - 複数回実行しても安全 (冪等) であること。

#![forbid(unsafe_code)]

use sakurasato_core::model::Visibility;
use sakurasato_core::repo;
use sakurasato_core::repo::actor::NewActor;
use sakurasato_core::repo::note::NewNote;
use sqlx::PgPool;

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
            socket: "/tmp/sakurasato-emoji-backfill-test-nonexistent.sock".into(),
            max_bytes: 1024,
            max_pixels: 1024,
            video: sakurasato_core::config::VideoConfig::default(),
            emoji_import: sakurasato_core::config::EmojiImportConfig::default(),
        },
        miauth: None,
    }
}

fn remote_actor(host: &str, user: &str) -> NewActor {
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
        shared_inbox_url: None,
        outbox_url: None,
        followers_url: None,
        following_url: None,
        public_key_id: format!("{ap_id}#main-key"),
        public_key_pem: "dummy".into(),
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

fn local_actor(host: &str, user: &str) -> NewActor {
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
        public_key_pem: "dummy".into(),
        private_key_pem: Some("dummy".into()),
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

fn new_note(suffix: &str, actor_id: i64, is_local: bool, tags: serde_json::Value) -> NewNote {
    NewNote {
        ap_id: format!("https://x.test/notes/{suffix}"),
        actor_id,
        content: "n".into(),
        language: None,
        in_reply_to_ap_id: None,
        in_reply_to_note_id: None,
        summary: None,
        visibility: Visibility::Public,
        sensitive: false,
        to_recipients: vec![],
        cc_recipients: vec![],
        attachments: serde_json::json!([]),
        tags,
        is_local,
        url: None,
        published_at: chrono::Utc::now(),
    }
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn backfill_learns_emoji_from_remote_notes_only(pool: PgPool) {
    let local = repo::actor::insert(&pool, local_actor("sakura.test", "alice"))
        .await
        .unwrap();
    let remote = repo::actor::insert(&pool, remote_actor("remote.test", "bob"))
        .await
        .unwrap();

    let emoji_tag = serde_json::json!([
        {"type": "Emoji", "id": "https://remote.test/emojis/blob",
         "name": ":blob:", "icon": {"url": "https://remote.test/files/blob.png", "mediaType": "image/png"}},
    ]);

    // ローカル Note (tags 有) → 対象外。
    repo::note::insert(&pool, new_note("local", local.id, true, emoji_tag.clone()))
        .await
        .unwrap();
    // リモート Note、tags 空 → 対象外。
    repo::note::insert(
        &pool,
        new_note("empty-tags", remote.id, false, serde_json::json!([])),
    )
    .await
    .unwrap();
    // リモート Note、tags 有 → 対象。
    repo::note::insert(&pool, new_note("with-emoji", remote.id, false, emoji_tag))
        .await
        .unwrap();

    let state =
        sakurasato_server::state::AppState::from_pool(pool.clone(), make_config("sakura.test"));

    let summary = sakurasato_server::emoji_backfill::run_with_state(&state)
        .await
        .unwrap();
    assert_eq!(
        summary.scanned, 1,
        "only the remote note with tags is scanned"
    );
    assert_eq!(summary.emoji_tags_seen, 1);
    assert_eq!(summary.emoji_learned, 1);

    let learned = repo::emoji::get_by_ap_id(&pool, "https://remote.test/emojis/blob")
        .await
        .unwrap()
        .expect("emoji must be learned");
    assert_eq!(learned.shortcode, "blob");
    assert_eq!(learned.host.as_deref(), Some("remote.test"));
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn backfill_is_idempotent_across_multiple_runs(pool: PgPool) {
    let remote = repo::actor::insert(&pool, remote_actor("remote.test", "carol"))
        .await
        .unwrap();
    let emoji_tag = serde_json::json!([
        {"type": "Emoji", "id": "https://remote.test/emojis/party",
         "name": ":party:", "icon": {"url": "https://remote.test/files/party.png", "mediaType": "image/png"}},
    ]);
    repo::note::insert(&pool, new_note("with-emoji-2", remote.id, false, emoji_tag))
        .await
        .unwrap();

    let state =
        sakurasato_server::state::AppState::from_pool(pool.clone(), make_config("sakura.test"));

    let first = sakurasato_server::emoji_backfill::run_with_state(&state)
        .await
        .unwrap();
    assert_eq!(first.emoji_learned, 1);

    // 2 回目も安全 (upsert_remote が ap_id に ON CONFLICT するため件数は変わらない)。
    let second = sakurasato_server::emoji_backfill::run_with_state(&state)
        .await
        .unwrap();
    assert_eq!(second.scanned, 1);
    assert_eq!(second.emoji_tags_seen, 1);
    assert_eq!(second.emoji_learned, 1);
}
