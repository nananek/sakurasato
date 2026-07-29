//! Issue #113 統合テスト: outbound Follow の重複 enqueue を防ぐ。
//!
//! `create_follow_core` を 2 回叩いて `delivery_queue` 行が **1 件のまま**
//! で、2 回目が `already_pending = true` で返ることを確認する。
//!
//! ## カバー範囲
//!
//! - 初回 follow → delivery_queue=1, follow.state=pending, queue_id=Some
//! - 2 回目 follow → delivery_queue=1 (= 増えない), already_pending=true,
//!   queue_id=None
//! - 3 回目以降も同様 (=冪等)
//! - accepted 状態に遷移後の follow → already_accepted=true, queue_id=None

#![forbid(unsafe_code)]
#![allow(clippy::doc_markdown, reason = "module doc に bullet で識別子を並べる")]

use sakurasato_core::model::FollowState;
use sakurasato_core::repo;
use sakurasato_core::repo::actor::NewActor;
use sakurasato_server::follow::{FollowTarget, create_follow_core};
use sakurasato_server::state::AppState;
use sqlx::PgPool;

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

fn local_actor() -> NewActor {
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
        shared_inbox_url: Some(format!("https://{host}/inbox")),
        outbox_url: None,
        followers_url: None,
        following_url: None,
        public_key_id: format!("{ap_id}#main-key"),
        public_key_pem: "-----BEGIN PUBLIC KEY-----\nMOCK-REMOTE\n-----END PUBLIC KEY-----".into(),
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

async fn count_queue_for_actor(pool: &PgPool, sender_actor_id: i64) -> i64 {
    let row: (i64,) =
        sqlx::query_as("SELECT count(*) FROM delivery_queue WHERE sender_actor_id = $1")
            .bind(sender_actor_id)
            .fetch_one(pool)
            .await
            .unwrap();
    row.0
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn follow_initial_call_enqueues_once(pool: PgPool) {
    let local = repo::actor::insert(&pool, local_actor()).await.unwrap();
    let bob = repo::actor::insert(&pool, remote_actor("remote.test", "bob"))
        .await
        .unwrap();
    let state = AppState::from_pool(pool.clone(), make_config());

    let outcome = create_follow_core(&state, FollowTarget::ActorId(bob.id))
        .await
        .unwrap();

    assert!(!outcome.already_accepted);
    assert!(!outcome.already_pending);
    assert!(
        outcome.queue_id.is_some(),
        "expected queued, got {outcome:?}"
    );
    assert_eq!(outcome.follow.state, FollowState::Pending.as_str());
    assert_eq!(count_queue_for_actor(&pool, local.id).await, 1);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn follow_second_call_does_not_enqueue_duplicate(pool: PgPool) {
    let local = repo::actor::insert(&pool, local_actor()).await.unwrap();
    let bob = repo::actor::insert(&pool, remote_actor("remote.test", "bob"))
        .await
        .unwrap();
    let state = AppState::from_pool(pool.clone(), make_config());

    let first = create_follow_core(&state, FollowTarget::ActorId(bob.id))
        .await
        .unwrap();
    assert!(first.queue_id.is_some());
    assert_eq!(count_queue_for_actor(&pool, local.id).await, 1);

    // 2 回目: pending 行が既存 → enqueue 抑止。
    let second = create_follow_core(&state, FollowTarget::ActorId(bob.id))
        .await
        .unwrap();
    assert!(
        second.already_pending,
        "second call should report already_pending: {second:?}"
    );
    assert!(!second.already_accepted);
    assert!(second.queue_id.is_none());
    assert_eq!(
        count_queue_for_actor(&pool, local.id).await,
        1,
        "delivery_queue must stay at 1 row after second follow call",
    );

    // 3 回目以降も同じ挙動 (冪等)。
    let third = create_follow_core(&state, FollowTarget::ActorId(bob.id))
        .await
        .unwrap();
    assert!(third.already_pending);
    assert_eq!(count_queue_for_actor(&pool, local.id).await, 1);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn follow_after_accepted_returns_already_accepted(pool: PgPool) {
    let _ = repo::actor::insert(&pool, local_actor()).await.unwrap();
    let bob = repo::actor::insert(&pool, remote_actor("remote.test", "bob"))
        .await
        .unwrap();
    let state = AppState::from_pool(pool.clone(), make_config());

    // 1 回目: pending で enqueue。
    let first = create_follow_core(&state, FollowTarget::ActorId(bob.id))
        .await
        .unwrap();
    let follow_id = first.follow.id;

    // 模擬: 相手から Accept が返って accepted に遷移。
    repo::follow::set_state(&pool, follow_id, FollowState::Accepted)
        .await
        .unwrap();

    // 2 回目: already_accepted で返る。enqueue は増えない (= 元の 1 件のまま)。
    let second = create_follow_core(&state, FollowTarget::ActorId(bob.id))
        .await
        .unwrap();
    assert!(
        second.already_accepted,
        "second call after accept should report already_accepted: {second:?}"
    );
    assert!(!second.already_pending);
    assert!(second.queue_id.is_none());
}
