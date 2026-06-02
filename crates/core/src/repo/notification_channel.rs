//! `notification_channel` テーブル (migration 0013 + 0014) の CRUD。
//!
//! Discord (および Slack / Misskey 互換) webhook 通知の宛先。実 POST は
//! `delivery_queue` 経由で worker が撃つ ── この repo 自体は HTTP を一切
//! 触らない。`list_enabled_for_event` だけ「当該 `notify_*` が TRUE」を
//! WHERE で絞って返す高頻度 path 用。
//!
//! migration 0014 で master `enabled` 列は撤去 (元は 2 段スイッチだったが
//! `--event all` の直感と衝突し `toggle` が冪等にならなかったため)。
//! 全停止は `set_events(_, &NotificationEvent::all(), false)` で 7 個
//! `notify_*` を一斉 FALSE する運用に統一。`enable --only` の完全宣言
//! モードでは [`set_exact_state`] が指定 event だけ TRUE / 他 FALSE に倒す。
//!
//! `column_name` を動的に WHERE / SET に埋めているため、`NotificationEvent`
//! enum を経由しない生文字列を受け取ってはならない (= SQL injection 防御)。
//! enum 値は静的なので注入経路はない。

use sqlx::{AssertSqlSafe, PgPool};

use crate::model::{NotificationChannelRow, NotificationEvent};

/// `notification_channel` への INSERT 引数。`notify_*` は default
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
            id, name, url, format,
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
            id, name, url, format,
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

/// 7 個の `notify_*` 列をすべて宣言的に設定する (idempotent + atomic)。
/// CLI `enable --only mention,quote` の本体 ── 指定 event は TRUE、
/// それ以外は FALSE に倒す。「希望状態を 1 コマンドで言い切る」用途。
///
/// `enabled` 集合 (= TRUE にする event) を渡す。`NotificationEvent` は `Eq`
/// なので集合操作は線形探索で十分 (= 高々 7 要素)。
///
/// row があったかを bool で返す。
pub async fn set_exact_state(
    pool: &PgPool,
    id: i64,
    enabled: &[NotificationEvent],
) -> sqlx::Result<bool> {
    let val_for = |ev: NotificationEvent| -> bool { enabled.contains(&ev) };
    let res = sqlx::query!(
        r#"
        UPDATE notification_channel
        SET
            notify_mention = $2,
            notify_direct = $3,
            notify_quote = $4,
            notify_reaction = $5,
            notify_renote = $6,
            notify_follow = $7,
            notify_follow_request = $8,
            updated_at = now()
        WHERE id = $1
        "#,
        id,
        val_for(NotificationEvent::Mention),
        val_for(NotificationEvent::Direct),
        val_for(NotificationEvent::Quote),
        val_for(NotificationEvent::Reaction),
        val_for(NotificationEvent::Renote),
        val_for(NotificationEvent::Follow),
        val_for(NotificationEvent::FollowRequest),
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// 複数の `notify_<event>` 列を 1 つの UPDATE 文で一斉に `value` に設定する
/// (idempotent + atomic)。CLI `enable mention,quote` 等のリスト経路で使う。
///
/// `events` は **空でないこと** を呼び出し側が保証する (= clap の
/// `required = true` で保証される)。重複した event は dedup して最終 SQL の
/// `SET col = $2, col = $2` 重複を防ぐ ── `PostgreSQL` は同じ列への重複代入を
/// パースエラーで弾く ("column ... specified more than once") のでガードが必要。
///
/// 列名は [`NotificationEvent::column_name`] 由来 (= 静的 &str) なので SQL
/// injection の入口はない。`query_as!` は使えないが SQL 自体は `$1` (id) と
/// `$2` (value) の 2 個のみ bind されパース後に固定されるので prepared
/// statement キャッシュも効く。
///
/// row があったかを bool で返す。同じ値を再設定しても `rows_affected = 1`
/// なので「存在する/しない」のシグナルとして使える。
pub async fn set_events(
    pool: &PgPool,
    id: i64,
    events: &[NotificationEvent],
    value: bool,
) -> sqlx::Result<bool> {
    if events.is_empty() {
        // 上位で防いでいるはずだが念のため: 空集合の UPDATE は何もせず not-found
        // と区別がつかなくなるので呼ばないこと。
        return Ok(false);
    }
    // 重複除去 (順序保持)。NotificationEvent は Copy + Eq。
    let mut deduped: Vec<NotificationEvent> = Vec::with_capacity(events.len());
    for ev in events {
        if !deduped.contains(ev) {
            deduped.push(*ev);
        }
    }
    let set_clause = deduped
        .iter()
        .map(|ev| format!("{} = $2", ev.column_name()))
        .collect::<Vec<_>>()
        .join(", ");
    let sql =
        format!("UPDATE notification_channel SET {set_clause}, updated_at = now() WHERE id = $1");
    let res = sqlx::query(AssertSqlSafe(sql))
        .bind(id)
        .bind(value)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// `notify_<event> = TRUE` な行を全部返す。dispatch hook の fan-out で
/// 呼ばれる高頻度 path。`column_name` を動的に埋め込むが、`event` は enum
/// 由来 (= 静的 &str) なので injection はない。
pub async fn list_enabled_for_event(
    pool: &PgPool,
    event: NotificationEvent,
) -> sqlx::Result<Vec<NotificationChannelRow>> {
    let sql = format!(
        r"
        SELECT
            id, name, url, format,
            notify_mention, notify_direct, notify_quote, notify_reaction,
            notify_renote, notify_follow, notify_follow_request,
            created_at, updated_at
        FROM notification_channel
        WHERE {col} = TRUE
        ORDER BY id ASC
        ",
        col = event.column_name(),
    );
    sqlx::query_as::<_, NotificationChannelRow>(AssertSqlSafe(sql))
        .fetch_all(pool)
        .await
}
