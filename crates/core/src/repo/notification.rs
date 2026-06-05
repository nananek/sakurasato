//! `notification` テーブル (migration 0020) の in-app 通知フィード CRUD。
//!
//! 生成は `crate::notification::dispatch::notify` (server crate) が webhook
//! enqueue と並行して [`insert`] を呼ぶ。読み出しは TUI の local API と `MiAuth` の
//! `/api/i/notifications` が [`list`] / [`count_unread`] を、既読操作は
//! [`mark_read`] / [`mark_all_read`] を使う。
//!
//! ページングは Misskey 互換で **`sinceId` / `untilId` 排他** ── `id > sinceId`
//! / `id < untilId`。並びは `id DESC` (= 新しい順)。

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::model::NotificationRow;

/// `notification` への INSERT 引数。
#[derive(Debug, Clone)]
pub struct NewNotification {
    pub recipient_actor_id: i64,
    /// `NotificationEvent::as_str()` の値 (`snake_case`)。
    pub event_type: String,
    pub notifier_actor_id: Option<i64>,
    pub note_id: Option<i64>,
    pub reaction: Option<String>,
    /// 発生時刻 (= `NotificationContext::occurred_at`)。テストで固定値を渡せる。
    pub created_at: DateTime<Utc>,
}

pub async fn insert(pool: &PgPool, new: NewNotification) -> sqlx::Result<NotificationRow> {
    sqlx::query_as!(
        NotificationRow,
        r#"
        INSERT INTO notification
            (recipient_actor_id, event_type, notifier_actor_id, note_id, reaction, created_at)
        VALUES ($1, $2, $3, $4, $5, $6)
        RETURNING
            id, recipient_actor_id, event_type, notifier_actor_id, note_id, reaction,
            is_read, created_at
        "#,
        new.recipient_actor_id,
        new.event_type,
        new.notifier_actor_id,
        new.note_id,
        new.reaction,
        new.created_at,
    )
    .fetch_one(pool)
    .await
}

/// `recipient` の通知を `id DESC` で返す。`since_id` / `until_id` は **排他**
/// (= Misskey の `sinceId` / `untilId` 準拠)。`limit` は呼び出し側で clamp 済み想定。
pub async fn list(
    pool: &PgPool,
    recipient_actor_id: i64,
    limit: i64,
    since_id: Option<i64>,
    until_id: Option<i64>,
) -> sqlx::Result<Vec<NotificationRow>> {
    sqlx::query_as!(
        NotificationRow,
        r#"
        SELECT
            id, recipient_actor_id, event_type, notifier_actor_id, note_id, reaction,
            is_read, created_at
        FROM notification
        WHERE recipient_actor_id = $1
          AND ($2::bigint IS NULL OR id > $2)
          AND ($3::bigint IS NULL OR id < $3)
        ORDER BY id DESC
        LIMIT $4
        "#,
        recipient_actor_id,
        since_id,
        until_id,
        limit,
    )
    .fetch_all(pool)
    .await
}

/// `recipient` の未読件数。
pub async fn count_unread(pool: &PgPool, recipient_actor_id: i64) -> sqlx::Result<i64> {
    sqlx::query_scalar!(
        r#"SELECT count(*) FROM notification WHERE recipient_actor_id = $1 AND is_read = FALSE"#,
        recipient_actor_id,
    )
    .fetch_one(pool)
    .await
    .map(|c| c.unwrap_or(0))
}

/// 単一通知を既読化。`recipient` 不一致 / 既に既読 / 不在なら `false`。
pub async fn mark_read(pool: &PgPool, recipient_actor_id: i64, id: i64) -> sqlx::Result<bool> {
    let res = sqlx::query!(
        r#"
        UPDATE notification SET is_read = TRUE
        WHERE id = $1 AND recipient_actor_id = $2 AND is_read = FALSE
        "#,
        id,
        recipient_actor_id,
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// `recipient` の未読を全件既読化。既読化した件数を返す。
pub async fn mark_all_read(pool: &PgPool, recipient_actor_id: i64) -> sqlx::Result<u64> {
    let res = sqlx::query!(
        r#"UPDATE notification SET is_read = TRUE WHERE recipient_actor_id = $1 AND is_read = FALSE"#,
        recipient_actor_id,
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}
