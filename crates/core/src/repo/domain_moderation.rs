//! Compile-time checked queries against the `domain_moderation` table。
//!
//! ドメイン (host) 単位のモデレーション状態 (`silence` / `suspend`)。行が
//! 存在しない = 通常運用。`host` は `actor.host` と同じ表現 (小文字正規化
//! 済みホスト名) を前提にする ── 呼び出し側で正規化してから渡すこと。

use sqlx::PgPool;

use crate::model::{ActorRow, DomainModerationRow};
use crate::repo::follow::FollowWithActor;

/// 措置を設定/変更する。既存行があれば `severity`/`reason` を上書きする。
pub async fn upsert(
    pool: &PgPool,
    host: &str,
    severity: &str,
    reason: Option<&str>,
) -> sqlx::Result<DomainModerationRow> {
    sqlx::query_as!(
        DomainModerationRow,
        r#"
        INSERT INTO domain_moderation (host, severity, reason)
        VALUES ($1, $2, $3)
        ON CONFLICT (host) DO UPDATE
            SET severity = EXCLUDED.severity,
                reason = EXCLUDED.reason,
                updated_at = now()
        RETURNING id, host, severity, reason, created_at, updated_at
        "#,
        host,
        severity,
        reason,
    )
    .fetch_one(pool)
    .await
}

/// inbox 受信ガード・配送ガードのホットパスで使う。`host` は正規化済みで
/// 渡すこと (呼び出し側が `eq_ignore_ascii_case` ではなく完全一致で引く)。
pub async fn get_by_host(pool: &PgPool, host: &str) -> sqlx::Result<Option<DomainModerationRow>> {
    sqlx::query_as!(
        DomainModerationRow,
        r#"
        SELECT id, host, severity, reason, created_at, updated_at
        FROM domain_moderation WHERE host = $1
        "#,
        host,
    )
    .fetch_optional(pool)
    .await
}

/// 措置解除。戻り値は影響行数 (0 or 1)。
pub async fn delete_by_host(pool: &PgPool, host: &str) -> sqlx::Result<u64> {
    let res = sqlx::query!("DELETE FROM domain_moderation WHERE host = $1", host)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// 設定済み一覧。`host` 昇順。
pub async fn list(pool: &PgPool) -> sqlx::Result<Vec<DomainModerationRow>> {
    sqlx::query_as!(
        DomainModerationRow,
        r#"
        SELECT id, host, severity, reason, created_at, updated_at
        FROM domain_moderation ORDER BY host ASC
        "#,
    )
    .fetch_all(pool)
    .await
}

/// TUI ドメイン管理画面 / CLI `domain info` 用の統計。
#[derive(Debug, Clone, Copy, Default)]
pub struct DomainStats {
    pub known_actor_count: i64,
    pub accepted_following_count: i64,
    pub accepted_followers_count: i64,
    pub pending_following_count: i64,
    pub pending_followers_count: i64,
}

/// `host` に属する actor 数と、`local_actor_id` から見た following/followers
/// の accepted / pending 件数をまとめて返す。
pub async fn domain_stats(
    pool: &PgPool,
    local_actor_id: i64,
    host: &str,
) -> sqlx::Result<DomainStats> {
    let row = sqlx::query!(
        r#"
        SELECT
            (SELECT count(*) FROM actor WHERE host = $2 AND is_local = FALSE) AS "known_actor_count!",
            (SELECT count(*) FROM follow f JOIN actor a ON a.id = f.followed_actor_id
                WHERE f.follower_actor_id = $1 AND a.host = $2 AND f.state = 'accepted') AS "accepted_following_count!",
            (SELECT count(*) FROM follow f JOIN actor a ON a.id = f.follower_actor_id
                WHERE f.followed_actor_id = $1 AND a.host = $2 AND f.state = 'accepted') AS "accepted_followers_count!",
            (SELECT count(*) FROM follow f JOIN actor a ON a.id = f.followed_actor_id
                WHERE f.follower_actor_id = $1 AND a.host = $2 AND f.state = 'pending') AS "pending_following_count!",
            (SELECT count(*) FROM follow f JOIN actor a ON a.id = f.follower_actor_id
                WHERE f.followed_actor_id = $1 AND a.host = $2 AND f.state = 'pending') AS "pending_followers_count!"
        "#,
        local_actor_id,
        host,
    )
    .fetch_one(pool)
    .await?;
    Ok(DomainStats {
        known_actor_count: row.known_actor_count,
        accepted_following_count: row.accepted_following_count,
        accepted_followers_count: row.accepted_followers_count,
        pending_following_count: row.pending_following_count,
        pending_followers_count: row.pending_followers_count,
    })
}

/// local actor が `host` 内の相手を follow している行を列挙する
/// (`repo::follow::list_following` の host フィルタ版)。**state は問わず全件**
/// 返す ── suspend 実行コアロジックが `pending` 行も含めて強制解除の対象に
/// するため (`crate::repo::follow::FollowWithActor` を再利用)。
pub async fn list_following_in_domain(
    pool: &PgPool,
    local_actor_id: i64,
    host: &str,
) -> sqlx::Result<Vec<FollowWithActor>> {
    let rows = sqlx::query!(
        r#"
        SELECT
            f.id           AS "follow_id!",
            f.state        AS "follow_state!",
            f.created_at   AS "follow_created_at!",
            a.id           AS "actor_id!",
            a.ap_id        AS "actor_ap_id!",
            a.preferred_username,
            a.host,
            a.display_name,
            a.summary,
            a.icon_url,
            a.image_url,
            a.inbox_url,
            a.shared_inbox_url,
            a.outbox_url,
            a.followers_url,
            a.following_url,
            a.public_key_id,
            a.public_key_pem,
            a.ed25519_public_key_id,
            a.ed25519_public_key_pem,
            a.also_known_as as "also_known_as: sqlx::types::Json<Vec<String>>",
            a.moved_to_ap_id,
            a.is_local,
            a.actor_type,
            a.manually_approves_followers,
            a.birthday,
            a.location,
            a.lang,
            a.followed_message,
            a.fields as "fields: sqlx::types::Json<Vec<crate::model::ActorField>>",
            a.followers_count, a.following_count, a.notes_count,
            a.fetched_at,
            a.created_at   AS "actor_created_at!",
            a.updated_at   AS "actor_updated_at!"
        FROM follow f
        JOIN actor a ON a.id = f.followed_actor_id
        WHERE f.follower_actor_id = $1 AND a.host = $2
        ORDER BY f.id DESC
        "#,
        local_actor_id,
        host,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| FollowWithActor {
            follow_id: r.follow_id,
            follow_state: r.follow_state,
            follow_created_at: r.follow_created_at,
            actor: ActorRow {
                id: r.actor_id,
                ap_id: r.actor_ap_id,
                preferred_username: r.preferred_username,
                host: r.host,
                display_name: r.display_name,
                summary: r.summary,
                icon_url: r.icon_url,
                image_url: r.image_url,
                inbox_url: r.inbox_url,
                shared_inbox_url: r.shared_inbox_url,
                outbox_url: r.outbox_url,
                followers_url: r.followers_url,
                following_url: r.following_url,
                public_key_id: r.public_key_id,
                public_key_pem: r.public_key_pem,
                private_key_pem: None,
                ed25519_public_key_id: r.ed25519_public_key_id,
                ed25519_public_key_pem: r.ed25519_public_key_pem,
                ed25519_private_key_pem: None,
                also_known_as: r.also_known_as,
                moved_to_ap_id: r.moved_to_ap_id,
                is_local: r.is_local,
                actor_type: r.actor_type,
                manually_approves_followers: r.manually_approves_followers,
                birthday: r.birthday,
                location: r.location,
                lang: r.lang,
                followed_message: r.followed_message,
                fields: r.fields,
                followers_count: r.followers_count,
                following_count: r.following_count,
                notes_count: r.notes_count,
                fetched_at: r.fetched_at,
                created_at: r.actor_created_at,
                updated_at: r.actor_updated_at,
            },
        })
        .collect())
}

/// local actor を `host` 内の相手が follow している行を列挙する
/// (`repo::follow::list_followers` の host フィルタ版)。[`list_following_in_domain`]
/// と同じく state は問わず全件返す。
pub async fn list_followers_in_domain(
    pool: &PgPool,
    local_actor_id: i64,
    host: &str,
) -> sqlx::Result<Vec<FollowWithActor>> {
    let rows = sqlx::query!(
        r#"
        SELECT
            f.id           AS "follow_id!",
            f.state        AS "follow_state!",
            f.created_at   AS "follow_created_at!",
            a.id           AS "actor_id!",
            a.ap_id        AS "actor_ap_id!",
            a.preferred_username,
            a.host,
            a.display_name,
            a.summary,
            a.icon_url,
            a.image_url,
            a.inbox_url,
            a.shared_inbox_url,
            a.outbox_url,
            a.followers_url,
            a.following_url,
            a.public_key_id,
            a.public_key_pem,
            a.ed25519_public_key_id,
            a.ed25519_public_key_pem,
            a.also_known_as as "also_known_as: sqlx::types::Json<Vec<String>>",
            a.moved_to_ap_id,
            a.is_local,
            a.actor_type,
            a.manually_approves_followers,
            a.birthday,
            a.location,
            a.lang,
            a.followed_message,
            a.fields as "fields: sqlx::types::Json<Vec<crate::model::ActorField>>",
            a.followers_count, a.following_count, a.notes_count,
            a.fetched_at,
            a.created_at   AS "actor_created_at!",
            a.updated_at   AS "actor_updated_at!"
        FROM follow f
        JOIN actor a ON a.id = f.follower_actor_id
        WHERE f.followed_actor_id = $1 AND a.host = $2
        ORDER BY f.id DESC
        "#,
        local_actor_id,
        host,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| FollowWithActor {
            follow_id: r.follow_id,
            follow_state: r.follow_state,
            follow_created_at: r.follow_created_at,
            actor: ActorRow {
                id: r.actor_id,
                ap_id: r.actor_ap_id,
                preferred_username: r.preferred_username,
                host: r.host,
                display_name: r.display_name,
                summary: r.summary,
                icon_url: r.icon_url,
                image_url: r.image_url,
                inbox_url: r.inbox_url,
                shared_inbox_url: r.shared_inbox_url,
                outbox_url: r.outbox_url,
                followers_url: r.followers_url,
                following_url: r.following_url,
                public_key_id: r.public_key_id,
                public_key_pem: r.public_key_pem,
                private_key_pem: None,
                ed25519_public_key_id: r.ed25519_public_key_id,
                ed25519_public_key_pem: r.ed25519_public_key_pem,
                ed25519_private_key_pem: None,
                also_known_as: r.also_known_as,
                moved_to_ap_id: r.moved_to_ap_id,
                is_local: r.is_local,
                actor_type: r.actor_type,
                manually_approves_followers: r.manually_approves_followers,
                birthday: r.birthday,
                location: r.location,
                lang: r.lang,
                followed_message: r.followed_message,
                fields: r.fields,
                followers_count: r.followers_count,
                following_count: r.following_count,
                notes_count: r.notes_count,
                fetched_at: r.fetched_at,
                created_at: r.actor_created_at,
                updated_at: r.actor_updated_at,
            },
        })
        .collect())
}

/// `host` 別の既知 actor 数一覧 + 現在の moderation state
/// (TUI ドメイン一覧画面のトップレベル)。
#[derive(Debug, Clone)]
pub struct DomainSummary {
    pub host: String,
    pub actor_count: i64,
    pub severity: Option<String>,
}

/// remote actor を持つ全ドメインを actor 数降順で列挙し、`domain_moderation`
/// を LEFT JOIN して現在の severity を添える。
pub async fn list_known_hosts(pool: &PgPool) -> sqlx::Result<Vec<DomainSummary>> {
    let rows = sqlx::query!(
        r#"
        SELECT a.host AS "host!", count(*) AS "actor_count!", dm.severity AS "severity?"
        FROM actor a
        LEFT JOIN domain_moderation dm ON dm.host = a.host
        WHERE a.is_local = FALSE
        GROUP BY a.host, dm.severity
        ORDER BY count(*) DESC, a.host ASC
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| DomainSummary {
            host: r.host,
            actor_count: r.actor_count,
            severity: r.severity,
        })
        .collect())
}
