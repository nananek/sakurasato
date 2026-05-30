//! Compile-time checked queries against the `reaction` table.

use sqlx::PgPool;

use crate::model::ReactionRow;

pub async fn insert(
    pool: &PgPool,
    ap_id: &str,
    note_id: i64,
    actor_id: i64,
    content: &str,
    emoji_id: Option<i64>,
) -> sqlx::Result<ReactionRow> {
    sqlx::query_as!(
        ReactionRow,
        r#"
        INSERT INTO reaction (ap_id, note_id, actor_id, content, emoji_id)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id, ap_id, note_id, actor_id, content, emoji_id, created_at
        "#,
        ap_id,
        note_id,
        actor_id,
        content,
        emoji_id,
    )
    .fetch_one(pool)
    .await
}

/// `ap_id` で 1 行引く。Inbound `Undo` の対象確認に使う (M8 PR2)。
pub async fn get_by_ap_id(pool: &PgPool, ap_id: &str) -> sqlx::Result<Option<ReactionRow>> {
    sqlx::query_as!(
        ReactionRow,
        r#"
        SELECT id, ap_id, note_id, actor_id, content, emoji_id, created_at
        FROM reaction WHERE ap_id = $1
        "#,
        ap_id,
    )
    .fetch_optional(pool)
    .await
}

/// Idempotent insert (M8 PR2 inbound reaction)。
///
/// 同じ `ap_id` で再送された Activity は同じ行を返す (= リトライ安全)。
/// 別 `ap_id` だが natural key (`note_id`, `actor_id`, `content`) が衝突する
/// 場合 (= 同じ actor が同じ note に同じ content を別 Activity ID で押し付け
/// てきた) も既存行を返す。
///
/// 実装: Postgres の `ON CONFLICT (col)` は 1 つの制約しか同時に指定できない
/// ため、`ON CONFLICT DO NOTHING` (= 任意の衝突を抑える) + RETURNING で空が
/// 返ってきたら fallback SELECT で既存行を引く。
pub async fn insert_or_get(
    pool: &PgPool,
    ap_id: &str,
    note_id: i64,
    actor_id: i64,
    content: &str,
    emoji_id: Option<i64>,
) -> sqlx::Result<ReactionRow> {
    let inserted = sqlx::query_as!(
        ReactionRow,
        r#"
        INSERT INTO reaction (ap_id, note_id, actor_id, content, emoji_id)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT DO NOTHING
        RETURNING id, ap_id, note_id, actor_id, content, emoji_id, created_at
        "#,
        ap_id,
        note_id,
        actor_id,
        content,
        emoji_id,
    )
    .fetch_optional(pool)
    .await?;
    if let Some(row) = inserted {
        return Ok(row);
    }
    // 衝突 (ap_id か natural key のいずれか) で挿入できなかった。
    // 同じ ap_id を優先して引き、無ければ natural key で引く ──
    // 同じ Activity の再送だった場合に ap_id 一致行を返したいため。
    if let Some(row) = sqlx::query_as!(
        ReactionRow,
        r#"
        SELECT id, ap_id, note_id, actor_id, content, emoji_id, created_at
        FROM reaction WHERE ap_id = $1
        "#,
        ap_id,
    )
    .fetch_optional(pool)
    .await?
    {
        return Ok(row);
    }
    sqlx::query_as!(
        ReactionRow,
        r#"
        SELECT id, ap_id, note_id, actor_id, content, emoji_id, created_at
        FROM reaction WHERE note_id = $1 AND actor_id = $2 AND content = $3
        "#,
        note_id,
        actor_id,
        content,
    )
    .fetch_one(pool)
    .await
}

pub async fn delete_by_ap_id(pool: &PgPool, ap_id: &str) -> sqlx::Result<u64> {
    Ok(sqlx::query!("DELETE FROM reaction WHERE ap_id = $1", ap_id)
        .execute(pool)
        .await?
        .rows_affected())
}

/// `note_id` に紐付くリアクションを `(content, count)` で集約して返す。
///
/// TUI / Web 表示用 (M8 PR3 で TUI から消費)。content は AP のまま (= `:foo:`
/// 形式や Unicode emoji がそのまま入る)。`emoji_id` の resolution は呼び出し側
/// 責務 ── 連合先によって `emoji_id` の有無が変わるため、表示層で別途引く。
pub async fn count_by_note(pool: &PgPool, note_id: i64) -> sqlx::Result<Vec<ReactionContentCount>> {
    sqlx::query_as!(
        ReactionContentCount,
        r#"
        SELECT
            content as "content!",
            COUNT(*) as "count!",
            MAX(emoji_id) as "any_emoji_id: i64"
        FROM reaction
        WHERE note_id = $1
        GROUP BY content
        ORDER BY MIN(created_at)
        "#,
        note_id,
    )
    .fetch_all(pool)
    .await
}

/// `count_by_note` の結果型。`(content, count, 代表 emoji_id)`。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ReactionContentCount {
    pub content: String,
    pub count: i64,
    /// `MAX(emoji_id)` を「代表値」として返す。同じ content (= 同じ shortcode)
    /// に複数の `emoji_id` が紐付くことは想定しないが、片方 NULL / 片方
    /// 既知の絵文字 → 既知側を採用したいので MAX を使う。
    pub any_emoji_id: Option<i64>,
}
