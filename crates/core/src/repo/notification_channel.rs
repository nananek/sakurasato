//! `notification_channel` テーブル (migration 0013) の CRUD。
//!
//! Discord (および Slack / Misskey 互換) webhook 通知の宛先。実 POST は
//! `delivery_queue` 経由で worker が撃つ ── この repo 自体は HTTP を一切
//! 触らない。`list_enabled_for_event` だけ「`enabled` AND 当該 `notify_*`」を
//! WHERE で絞って返す高頻度 path 用。
//!
//! `column_name` を動的に WHERE に埋めているため、`NotificationEvent` enum
//! を経由しない生文字列を受け取ってはならない (= SQL injection 防御)。enum
//! 値は静的なので注入経路はない。

use sqlx::{AssertSqlSafe, PgPool};

use crate::model::{NotificationChannelRow, NotificationEvent};

/// `notification_channel` への INSERT 引数。`enabled` / `notify_*` は default
/// TRUE なので渡さない (= まずは全イベントを通知する) 方針。
#[derive(Debug, Clone)]
pub struct NewNotificationChannel {
    pub name: String,
    pub url: String,
    /// `'embed'` または `'plain'`。`CHECK` 制約と整合する値を呼び出し側で
    /// 確定させる (CLI 側で `WebhookFormat::as_str()` 経由)。
    pub format: String,
}

pub async fn insert(
    pool: &PgPool,
    new: NewNotificationChannel,
) -> sqlx::Result<NotificationChannelRow> {
    sqlx::query_as!(
        NotificationChannelRow,
        r#"
        INSERT INTO notification_channel (name, url, format)
        VALUES ($1, $2, $3)
        RETURNING
            id,
            name,
            url,
            format,
            enabled,
            notify_mention,
            notify_direct,
            notify_quote,
            notify_reaction,
            notify_renote,
            notify_follow,
            notify_follow_request,
            created_at,
            updated_at
        "#,
        new.name,
        new.url,
        new.format,
    )
    .fetch_one(pool)
    .await
}

pub async fn list_all(pool: &PgPool) -> sqlx::Result<Vec<NotificationChannelRow>> {
    sqlx::query_as!(
        NotificationChannelRow,
        r#"
        SELECT
            id, name, url, format, enabled,
            notify_mention, notify_direct, notify_quote, notify_reaction,
            notify_renote, notify_follow, notify_follow_request,
            created_at, updated_at
        FROM notification_channel
        ORDER BY id ASC
        "#,
    )
    .fetch_all(pool)
    .await
}

pub async fn get_by_id(pool: &PgPool, id: i64) -> sqlx::Result<Option<NotificationChannelRow>> {
    sqlx::query_as!(
        NotificationChannelRow,
        r#"
        SELECT
            id, name, url, format, enabled,
            notify_mention, notify_direct, notify_quote, notify_reaction,
            notify_renote, notify_follow, notify_follow_request,
            created_at, updated_at
        FROM notification_channel
        WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await
}

/// `--id N` で hard delete。row があったかを bool で返す。
pub async fn delete_by_id(pool: &PgPool, id: i64) -> sqlx::Result<bool> {
    let res = sqlx::query!(r#"DELETE FROM notification_channel WHERE id = $1"#, id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// `enabled` 列を反転する。CLI `toggle --event all` で使う。row があったかを
/// 返す。
pub async fn toggle_enabled(pool: &PgPool, id: i64) -> sqlx::Result<bool> {
    let res = sqlx::query!(
        r#"
        UPDATE notification_channel
        SET enabled = NOT enabled, updated_at = now()
        WHERE id = $1
        "#,
        id,
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// `notify_<event>` 列を反転する。CLI `toggle --event mention|...` で使う。
///
/// 列名は [`NotificationEvent::column_name`] から取得した静的文字列なので
/// SQL injection の入口はない。`query_as!` を使えないので動的 SQL を組むが、
/// 列名は match で静的に決まる 7 種に限定される。
pub async fn toggle_event(pool: &PgPool, id: i64, event: NotificationEvent) -> sqlx::Result<bool> {
    // 列名は static &str のみ。ユーザ入力経路はないので AssertSqlSafe で
    // sqlx 0.9 の `SqlSafeStr` 要件を明示的に満たす。
    let sql = format!(
        "UPDATE notification_channel SET {col} = NOT {col}, updated_at = now() WHERE id = $1",
        col = event.column_name(),
    );
    let res = sqlx::query(AssertSqlSafe(sql))
        .bind(id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// `enabled = TRUE AND notify_<event> = TRUE` な行を全部返す。dispatch hook の
/// fan-out で呼ばれる高頻度 path。`column_name` を動的に埋め込むが、`event`
/// は enum 由来 (= 静的 &str) なので injection はない。
pub async fn list_enabled_for_event(
    pool: &PgPool,
    event: NotificationEvent,
) -> sqlx::Result<Vec<NotificationChannelRow>> {
    let sql = format!(
        r"
        SELECT
            id, name, url, format, enabled,
            notify_mention, notify_direct, notify_quote, notify_reaction,
            notify_renote, notify_follow, notify_follow_request,
            created_at, updated_at
        FROM notification_channel
        WHERE enabled = TRUE AND {col} = TRUE
        ORDER BY id ASC
        ",
        col = event.column_name(),
    );
    sqlx::query_as::<_, NotificationChannelRow>(AssertSqlSafe(sql))
        .fetch_all(pool)
        .await
}
