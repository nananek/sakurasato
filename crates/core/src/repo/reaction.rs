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

pub async fn delete_by_ap_id(pool: &PgPool, ap_id: &str) -> sqlx::Result<u64> {
    Ok(sqlx::query!("DELETE FROM reaction WHERE ap_id = $1", ap_id)
        .execute(pool)
        .await?
        .rows_affected())
}
