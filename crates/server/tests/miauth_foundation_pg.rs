//! M14 #157 (= 親 issue #150) — `MiAuth` foundation 統合テスト。
//!
//! `#[sqlx::test]` でパー DB を切り、`miauth_session` / `miauth_token` への
//! 直接 repo 呼び出し + `crate::miauth::auth` ヘルパの動作を確認する。
//! `GET /miauth/{uuid}` や `POST /api/miauth/{uuid}/check` の HTTP 経路は
//! #158 で乗るため、本 PR では handler 経路の統合テストは含めない。
//!
//! ## カバレッジ
//!
//! 1. token 発行 + hash lookup (`miauth approve` の中核経路)
//! 2. permission scope の `has_scope` 完全一致判定
//! 3. session 状態機械 (= pending → approved → consumed の CAS)
//! 4. reject 経路
//! 5. expire 経路 (= `expire_old_sessions` が pending 過期行を expired に倒す)
//! 6. 冪等な token re-fetch (`find_token_by_session`)
//! 7. session ↔ token の FK (= token revoke で `session.issued_token_id` が NULL)
//!
//! ## AGPL discipline
//!
//! 本テスト群は Sakurasato 側の repo / handler を叩くだけで、Misskey 本体
//! (= AGPL-3.0) には触らない。`misskey-py` を使う wire-compat parity test は
//! #158 以降の PR (= [`Issue #158`](https://github.com/nananek/sakurasato/issues/158)
//! 以降) で `tests/federation/` に追加される。

#![forbid(unsafe_code)]

use chrono::{Duration, Utc};
use sakurasato_core::model::MiAuthSessionState;
use sakurasato_core::repo;
use sakurasato_server::miauth::auth;
use sakurasato_server::state::AppState;
use sqlx::PgPool;
use uuid::Uuid;

mod common {
    use sakurasato_core::config::{
        DatabaseConfig, MediaProxyConfig, ServerConfig, ServerInfo, StorageConfig,
    };

    /// `#[sqlx::test]` から渡される `PgPool` で test 用 `Config` を組む。
    pub(super) fn make_config(host: &str) -> sakurasato_core::Config {
        sakurasato_core::Config {
            server: ServerConfig {
                host: host.into(),
                bind: "127.0.0.1:0".into(),
                local_api_socket: "/tmp/sakurasato.sock".into(),
                public_listen: None,
                local_api_listen: None,
                user: "alice".into(),
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
                video: sakurasato_core::config::VideoConfig::default(),
                emoji_import: sakurasato_core::config::EmojiImportConfig::default(),
            },
            // foundation #157 では Config に MiAuth を載せても serve 経路は
            // 走らない (= 統合テストは repo + ヘルパのみ叩く)。None で OK。
            miauth: None,
        }
    }
}

/// 既存 `tests/local_api_pg.rs` と同じ pattern で token を **直接 DB に積む**
/// ヘルパ。CLI 経路は別途 `tests/miauth_cli_pg.rs` 等で網羅する想定 (= #158
/// で session register endpoint が増えるタイミングで足す)。本 PR では repo
/// + ヘルパだけ確認する。
async fn issue_token(pool: &PgPool, name: &str, permissions: Vec<&str>) -> (String, i64) {
    let raw = sakurasato_server::token::generate_raw();
    let token_hash = sakurasato_server::token::hash(&raw);
    let row = repo::miauth::insert_token(
        pool,
        repo::miauth::NewMiAuthToken {
            name: name.into(),
            token_hash,
            permissions: permissions.into_iter().map(String::from).collect(),
        },
    )
    .await
    .expect("insert miauth_token");
    (raw, row.id)
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn token_insert_and_hash_lookup_roundtrip(pool: PgPool) {
    let state = AppState::from_pool(pool.clone(), common::make_config("example.test"));
    let (raw, token_id) = issue_token(
        &pool,
        "Milktea-test",
        vec!["read:account", "write:reactions"],
    )
    .await;

    // 1. 生 token で validate_token_raw が当該 row を返す。
    let row = auth::validate_token_raw(&state, &raw)
        .await
        .expect("token should validate");
    assert_eq!(row.id, token_id);
    assert_eq!(row.name, "Milktea-test");
    // permissions snapshot が CLI 入力どおり順序保持で返ってくる。
    assert_eq!(row.permissions.0, vec!["read:account", "write:reactions"]);

    // 2. 無関係な文字列は None を返す (= 401 経路)。
    let bogus = auth::validate_token_raw(&state, "nonexistent-raw-token").await;
    assert!(bogus.is_none(), "unknown raw token must not validate");

    // 3. permission scope は完全一致で判定される。
    assert!(auth::has_scope(&row, "read:account"));
    assert!(auth::has_scope(&row, "write:reactions"));
    assert!(!auth::has_scope(&row, "write:notes"));
    assert!(!auth::has_scope(&row, "read")); // prefix 無効
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn session_state_machine_happy_path(pool: PgPool) {
    let uuid = Uuid::new_v4();
    let now = Utc::now();
    let expires = now + Duration::seconds(600);

    // 1. browser landing で pending 行が作られる (= 後で #158 endpoint が叩く)。
    let inserted = repo::miauth::insert_session(
        &pool,
        repo::miauth::NewMiAuthSession {
            uuid,
            app_name: "Milktea".into(),
            callback_url: Some("milktea://oauth-callback".into()),
            // browser 側が要求した permission scope は session に snapshot。
            // CLI で approve 時に上書き可能 (= 下記)。
            permissions: vec!["read:account".into(), "write:notes".into()],
            expires_at: expires,
        },
    )
    .await
    .expect("insert pending session");
    assert_eq!(inserted.state_enum(), Some(MiAuthSessionState::Pending));
    assert!(inserted.issued_token_id.is_none());
    assert!(inserted.approved_at.is_none());

    // 2. CLI で approve すると state = 'approved' + permissions 上書き + approved_at セット。
    // ここでは「ユーザが client 要求より絞った」想定で write:notes を落とす。
    let permissions_after_approve = vec!["read:account".to_string()];
    let cas = repo::miauth::approve_session(&pool, uuid, &permissions_after_approve)
        .await
        .expect("approve CAS");
    assert_eq!(cas, 1, "first approve must transition exactly 1 row");

    let row = repo::miauth::get_session(&pool, uuid)
        .await
        .expect("get_session")
        .expect("row exists");
    assert_eq!(row.state_enum(), Some(MiAuthSessionState::Approved));
    assert_eq!(row.permissions.0, permissions_after_approve);
    assert!(row.approved_at.is_some());
    assert!(row.issued_token_id.is_none());

    // 3. 2 回目の approve は CAS で 0 を返す (= idempotent guard、Mastodon /
    //    Misskey の慣習に合わせて「すでに approved な session を再 approve」
    //    を冪等にしない ── 操作の事故を握り潰さないため)。
    let cas2 = repo::miauth::approve_session(&pool, uuid, &["something:else".into()])
        .await
        .expect("approve CAS 2");
    assert_eq!(cas2, 0, "double-approve must be rejected by CAS");

    // 4. token 発行 + consume 遷移。
    let (_raw, token_id) = issue_token(&pool, "Milktea", vec!["read:account"]).await;
    let cas3 = repo::miauth::mark_session_consumed(&pool, uuid, token_id)
        .await
        .expect("consume CAS");
    assert_eq!(cas3, 1, "first consume must transition exactly 1 row");

    let row = repo::miauth::get_session(&pool, uuid)
        .await
        .expect("get_session")
        .expect("row exists");
    assert_eq!(row.state_enum(), Some(MiAuthSessionState::Consumed));
    assert_eq!(row.issued_token_id, Some(token_id));
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn session_reject_transitions_pending_only(pool: PgPool) {
    let uuid = Uuid::new_v4();
    repo::miauth::insert_session(
        &pool,
        repo::miauth::NewMiAuthSession {
            uuid,
            app_name: "MissRirica".into(),
            callback_url: None,
            permissions: vec!["read:account".into()],
            expires_at: Utc::now() + Duration::seconds(600),
        },
    )
    .await
    .expect("insert");

    // pending → reject は 1。
    let cas = repo::miauth::reject_session(&pool, uuid)
        .await
        .expect("reject");
    assert_eq!(cas, 1);

    // rejected は終端、2 度目の reject は 0。
    let cas2 = repo::miauth::reject_session(&pool, uuid)
        .await
        .expect("reject 2");
    assert_eq!(cas2, 0);

    let row = repo::miauth::get_session(&pool, uuid)
        .await
        .expect("get")
        .expect("exists");
    assert_eq!(row.state_enum(), Some(MiAuthSessionState::Rejected));
    assert!(row.issued_token_id.is_none());
    assert!(row.approved_at.is_none());
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn expire_old_sessions_transitions_pending_past_expiry(pool: PgPool) {
    let uuid_fresh = Uuid::new_v4();
    let uuid_stale = Uuid::new_v4();

    // fresh: まだ期限内。
    repo::miauth::insert_session(
        &pool,
        repo::miauth::NewMiAuthSession {
            uuid: uuid_fresh,
            app_name: "Milktea".into(),
            callback_url: None,
            permissions: vec![],
            expires_at: Utc::now() + Duration::seconds(600),
        },
    )
    .await
    .expect("insert fresh");

    // stale: 既に期限切れの過去日時。
    repo::miauth::insert_session(
        &pool,
        repo::miauth::NewMiAuthSession {
            uuid: uuid_stale,
            app_name: "Milktea".into(),
            callback_url: None,
            permissions: vec![],
            expires_at: Utc::now() - Duration::seconds(10),
        },
    )
    .await
    .expect("insert stale");

    let expired = repo::miauth::expire_old_sessions(&pool)
        .await
        .expect("sweep");
    assert_eq!(expired, 1, "exactly 1 stale session must be expired");

    let fresh = repo::miauth::get_session(&pool, uuid_fresh)
        .await
        .expect("get fresh")
        .expect("exists");
    assert_eq!(
        fresh.state_enum(),
        Some(MiAuthSessionState::Pending),
        "fresh session must remain pending"
    );

    let stale = repo::miauth::get_session(&pool, uuid_stale)
        .await
        .expect("get stale")
        .expect("exists");
    assert_eq!(
        stale.state_enum(),
        Some(MiAuthSessionState::Expired),
        "stale session must be expired"
    );
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn list_pending_sweeps_before_returning(pool: PgPool) {
    let uuid_fresh = Uuid::new_v4();
    let uuid_stale = Uuid::new_v4();
    repo::miauth::insert_session(
        &pool,
        repo::miauth::NewMiAuthSession {
            uuid: uuid_fresh,
            app_name: "fresh".into(),
            callback_url: None,
            permissions: vec![],
            expires_at: Utc::now() + Duration::seconds(60),
        },
    )
    .await
    .unwrap();
    repo::miauth::insert_session(
        &pool,
        repo::miauth::NewMiAuthSession {
            uuid: uuid_stale,
            app_name: "stale".into(),
            callback_url: None,
            permissions: vec![],
            expires_at: Utc::now() - Duration::seconds(1),
        },
    )
    .await
    .unwrap();

    let pending = repo::miauth::list_pending_sessions(&pool)
        .await
        .expect("list");
    // stale は sweep されて出ない。
    assert_eq!(pending.len(), 1, "only fresh should remain pending");
    assert_eq!(pending[0].uuid, uuid_fresh);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn find_token_by_session_is_idempotent_for_repeat_check(pool: PgPool) {
    // POST /api/miauth/{uuid}/check の 2 度目以降が token を「再発行せず」
    // 同じものを返すことの基盤を検証する (= 実 endpoint は #158)。
    let uuid = Uuid::new_v4();
    repo::miauth::insert_session(
        &pool,
        repo::miauth::NewMiAuthSession {
            uuid,
            app_name: "client".into(),
            callback_url: None,
            permissions: vec!["read:account".into()],
            expires_at: Utc::now() + Duration::seconds(600),
        },
    )
    .await
    .unwrap();
    let cas = repo::miauth::approve_session(&pool, uuid, &["read:account".into()])
        .await
        .unwrap();
    assert_eq!(cas, 1);

    let (_, token_id) = issue_token(&pool, "client", vec!["read:account"]).await;
    let cas = repo::miauth::mark_session_consumed(&pool, uuid, token_id)
        .await
        .unwrap();
    assert_eq!(cas, 1);

    // 何度叩いても同じ token を返す (= 冪等)。
    let first = repo::miauth::find_token_by_session(&pool, uuid)
        .await
        .unwrap()
        .expect("token attached");
    let second = repo::miauth::find_token_by_session(&pool, uuid)
        .await
        .unwrap()
        .expect("token still attached");
    assert_eq!(first.id, second.id);
    assert_eq!(first.id, token_id);
}

#[sqlx::test(migrator = "sakurasato_core::MIGRATOR")]
async fn token_revoke_nulls_session_fk_but_keeps_session_row(pool: PgPool) {
    // 監査トレイル維持の検証: token を revoke しても session 行は残り、
    // ただし `issued_token_id` は NULL に倒れる (= ON DELETE SET NULL)。
    let uuid = Uuid::new_v4();
    repo::miauth::insert_session(
        &pool,
        repo::miauth::NewMiAuthSession {
            uuid,
            app_name: "client".into(),
            callback_url: None,
            permissions: vec!["read:account".into()],
            expires_at: Utc::now() + Duration::seconds(600),
        },
    )
    .await
    .unwrap();
    repo::miauth::approve_session(&pool, uuid, &["read:account".into()])
        .await
        .unwrap();
    let (_, token_id) = issue_token(&pool, "client", vec!["read:account"]).await;
    repo::miauth::mark_session_consumed(&pool, uuid, token_id)
        .await
        .unwrap();

    let deleted = repo::miauth::delete_token_by_id(&pool, token_id)
        .await
        .unwrap();
    assert!(deleted, "token row must be deleted");

    let row = repo::miauth::get_session(&pool, uuid)
        .await
        .unwrap()
        .expect("session row should survive token revoke");
    // state は consumed のまま (= history を変えない)。
    assert_eq!(row.state_enum(), Some(MiAuthSessionState::Consumed));
    // ただし FK は NULL に倒れている。
    assert!(
        row.issued_token_id.is_none(),
        "FK should be cleared by ON DELETE SET NULL"
    );

    // find_token_by_session は当然 None。
    let none = repo::miauth::find_token_by_session(&pool, uuid)
        .await
        .unwrap();
    assert!(none.is_none(), "no token left to find");
}
