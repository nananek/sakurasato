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

/// `(note_id, actor_id)` で 1 行引く ── ローカル user 自身の renote を
/// 取り消すときに使う (= TUI の `B` キー経路は activity id を覚えていなくても
/// 「自分が boost したやつ」を DB 側で解決できる)。`UNIQUE (note_id, actor_id)`
/// 制約があるので row は高々 1 件。
pub async fn get_by_pair(
    pool: &PgPool,
    note_id: i64,
    actor_id: i64,
) -> sqlx::Result<Option<AnnounceRow>> {
    sqlx::query_as!(
        AnnounceRow,
        r#"
        SELECT id, ap_id, note_id, actor_id, published_at, created_at
        FROM announce WHERE note_id = $1 AND actor_id = $2
        "#,
        note_id,
        actor_id,
    )
    .fetch_optional(pool)
    .await
}

/// 複数 Note に対する Announce (= boost) 集計を 1 クエリで取る。
///
/// `reaction::counts_for_notes` と同じ形式で home timeline 描画から呼ばれる。
/// `viewer_actor_id` 視点で「自分も renote したか」を `viewer_renoted` に
/// 載せ返す ── お一人様サーバなので viewer は常に local actor だが、
/// 引数として明示することで Test の安定性を確保する。
pub async fn counts_for_notes(
    pool: &PgPool,
    note_ids: &[i64],
    viewer_actor_id: i64,
) -> sqlx::Result<Vec<AnnounceSummaryRow>> {
    if note_ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as!(
        AnnounceSummaryRow,
        r#"
        SELECT
            note_id as "note_id!",
            COUNT(*) as "count!",
            BOOL_OR(actor_id = $2) as "viewer_renoted!"
        FROM announce
        WHERE note_id = ANY($1)
        GROUP BY note_id
        ORDER BY note_id
        "#,
        note_ids,
        viewer_actor_id,
    )
    .fetch_all(pool)
    .await
}

/// `counts_for_notes` の戻り行。`note_id` ごとに 1 行。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AnnounceSummaryRow {
    pub note_id: i64,
    pub count: i64,
    pub viewer_renoted: bool,
}
