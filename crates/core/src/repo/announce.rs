//! Compile-time checked queries against the `announce` table (M11).
//!
//! `Announce` (Boost) を受信したときに、誰がいつどの note を boost したかを
//! 記録する。`(note_id, actor_id)` UNIQUE で二重 boost は idempotent、
//! `ap_id` UNIQUE で `Undo` Announce のターゲット解決に使う。

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::model::AnnounceRow;

/// Insert an announce row, or return the existing one for the same
/// `(note_id, actor_id)` pair (idempotent — second receipt of the same
/// `Announce` from a peer's retry must not duplicate).
///
/// `ap_id` of the second receipt is ignored (we keep the first); peers that
/// re-announce with a different activity id are rare and the first-wins
/// rule keeps the Undo target stable.
pub async fn insert_or_get(
    pool: &PgPool,
    ap_id: &str,
    note_id: i64,
    actor_id: i64,
    published_at: DateTime<Utc>,
) -> sqlx::Result<AnnounceRow> {
    sqlx::query_as!(
        AnnounceRow,
        r#"
        INSERT INTO announce (ap_id, note_id, actor_id, published_at)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (note_id, actor_id) DO UPDATE
            SET published_at = announce.published_at
        RETURNING id, ap_id, note_id, actor_id, published_at, created_at
        "#,
        ap_id,
        note_id,
        actor_id,
        published_at,
    )
    .fetch_one(pool)
    .await
}

pub async fn get_by_ap_id(pool: &PgPool, ap_id: &str) -> sqlx::Result<Option<AnnounceRow>> {
    sqlx::query_as!(
        AnnounceRow,
        r#"
        SELECT id, ap_id, note_id, actor_id, published_at, created_at
        FROM announce WHERE ap_id = $1
        "#,
        ap_id,
    )
    .fetch_optional(pool)
    .await
}

/// Delete an announce row by AP id. Returns the number of rows deleted
/// (0 if the announce was never recorded — used by `Undo` for the
/// "we never had it" no-op case).
pub async fn delete_by_ap_id(pool: &PgPool, ap_id: &str) -> sqlx::Result<u64> {
    sqlx::query!("DELETE FROM announce WHERE ap_id = $1", ap_id)
        .execute(pool)
        .await
        .map(|r| r.rows_affected())
}
