//! `init::run_with_state` の DB 統合テスト (M13 小物 PR / Issue #73)。
//!
//! 鍵生成は実 RSA + Ed25519 を撃つので 1 ケース ~2-3 秒 ── 数を絞る。
//!
//! 検証する分岐:
//! - `[server].user` を変えて `--force` 無し → bail (stale local が居る)
//! - `[server].user` を変えて `--force` → 旧 actor 削除 + 新 actor 挿入、
//!   ローカル actor は常に 1 件
//! - lock 中の旧 actor を `--force` で user 変更 → 新 actor も lock 引き継ぎ

#![forbid(unsafe_code)]

use sakurasato_core::repo;
use sakurasato_core::repo::actor::NewActor;
use sakurasato_server::cli::InitArgs;
use sakurasato_server::init;
use sakurasato_server::state::AppState;
use sqlx::PgPool;

fn make_config(user: &str, host: &str) -> sakurasato_core::Config {
    sakurasato_core::Config {
        server: sakurasato_core::config::ServerConfig {
            host: host.into(),
            bind: "127.0.0.1:0".into(),
            local_api_socket: "/tmp/sakurasato.sock".into(),
            public_listen: None,
            local_api_listen: None,
            user: user.into(),
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

fn seed_local_actor(username: &str, host: &str, lock: bool) -> NewActor {
    let ap_id = format!("https://{host}/users/{username}");
    NewActor {
        ap_id: ap_id.clone(),
        preferred_username: username.into(),
        host: host.into(),
        display_name: Some(username.into()),
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
        ed25519_public_key_id: Some(format!("{ap_id}#ed25519-key")),
        ed25519_public_key_pem: Some(
            "-----BEGIN PUBLIC KEY-----\nMOCKED25\n-----END PUBLIC KEY-----".into(),
        ),
        ed25519_private_key_pem: Some(
            "-----BEGIN PRIVATE KEY-----\nMOCKED25\n-----END PRIVATE KEY-----".into(),
        ),
        also_known_as: vec![],
        moved_to_ap_id: None,
        is_local: true,
        actor_type: "Person".into(),
        manually_approves_followers: lock,
    }
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn init_bails_when_server_user_changed_without_force(pool: PgPool) {
    repo::actor::insert(&pool, seed_local_actor("alice", "example.test", false))
        .await
        .unwrap();
    let state = AppState::from_pool(pool, make_config("bob", "example.test"));

    let args = InitArgs {
        username: None,
        display_name: None,
        force: false,
        locked: false,
    };
    let err = init::run_with_state(&state, args).await.unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("stranded"), "msg={msg}");
    assert!(msg.contains("alice"), "msg={msg}");
    // 旧 actor は消えていない。
    let row = repo::actor::get_by_username_host(state.pool(), "alice", "example.test")
        .await
        .unwrap();
    assert!(row.is_some());
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn init_force_replaces_old_local_actor_on_user_change(pool: PgPool) {
    repo::actor::insert(&pool, seed_local_actor("alice", "example.test", false))
        .await
        .unwrap();
    let state = AppState::from_pool(pool, make_config("bob", "example.test"));

    let args = InitArgs {
        username: None,
        display_name: None,
        force: true,
        locked: false,
    };
    init::run_with_state(&state, args).await.unwrap();

    // 旧 alice は消え、新 bob が 1 件だけ存在する。
    assert!(
        repo::actor::get_by_username_host(state.pool(), "alice", "example.test")
            .await
            .unwrap()
            .is_none()
    );
    let bob = repo::actor::get_by_username_host(state.pool(), "bob", "example.test")
        .await
        .unwrap()
        .expect("new local actor bob must exist");
    assert!(bob.is_local);
    assert!(!bob.manually_approves_followers);
    let all = repo::actor::list_local(state.pool()).await.unwrap();
    assert_eq!(all.len(), 1, "exactly one local actor must remain");
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn init_force_user_change_inherits_lock_state(pool: PgPool) {
    repo::actor::insert(&pool, seed_local_actor("alice", "example.test", true))
        .await
        .unwrap();
    let state = AppState::from_pool(pool, make_config("bob", "example.test"));

    let args = InitArgs {
        username: None,
        display_name: None,
        force: true,
        locked: false, // ユーザは --locked 渡さないが、旧 actor が lock 済 → 引き継ぐ
    };
    init::run_with_state(&state, args).await.unwrap();

    let bob = repo::actor::get_by_username_host(state.pool(), "bob", "example.test")
        .await
        .unwrap()
        .expect("new local actor bob must exist");
    assert!(
        bob.manually_approves_followers,
        "lock state must be inherited across server.user change with --force",
    );
}
