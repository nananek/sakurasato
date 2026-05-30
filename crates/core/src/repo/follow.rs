//! Compile-time checked queries against the `follow` table.

// follow.{follower,followed}_actor_id naturally share a prefix; this is the
// AP terminology and aliasing would harm readability.
#![allow(clippy::similar_names)]

use sqlx::PgPool;

use crate::model::{FollowRow, FollowState};

pub async fn insert_pending(
    pool: &PgPool,
    ap_id: &str,
    follower_actor_id: i64,
    followed_actor_id: i64,
) -> sqlx::Result<FollowRow> {
    sqlx::query_as!(
        FollowRow,
        r#"
        INSERT INTO follow (ap_id, follower_actor_id, followed_actor_id, state)
        VALUES ($1, $2, $3, 'pending')
        RETURNING id, ap_id, follower_actor_id, followed_actor_id, state, created_at, updated_at
        "#,
        ap_id,
        follower_actor_id,
        followed_actor_id,
    )
    .fetch_one(pool)
    .await
}

pub async fn get_by_ap_id(pool: &PgPool, ap_id: &str) -> sqlx::Result<Option<FollowRow>> {
    sqlx::query_as!(
        FollowRow,
        r#"
        SELECT id, ap_id, follower_actor_id, followed_actor_id, state, created_at, updated_at
        FROM follow WHERE ap_id = $1
        "#,
        ap_id,
    )
    .fetch_optional(pool)
    .await
}

pub async fn set_state(pool: &PgPool, id: i64, state: FollowState) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE follow SET state = $1, updated_at = now() WHERE id = $2",
        state.as_str(),
        id,
    )
    .execute(pool)
    .await
    .map(|_| ())
}

/// Upsert a Follow row to `pending`. If a row with the same `ap_id` already
/// exists return it unchanged; otherwise insert a new pending row.
///
/// Mastodon retries inbound Follow on 5xx, so this must be idempotent — a
/// second delivery of the same Follow activity must not create a duplicate
/// row, and must not flip an already-`accepted` row back to `pending`.
pub async fn upsert_pending(
    pool: &PgPool,
    ap_id: &str,
    follower_actor_id: i64,
    followed_actor_id: i64,
) -> sqlx::Result<FollowRow> {
    if let Some(existing) = get_by_ap_id(pool, ap_id).await? {
        return Ok(existing);
    }
    // `(follower_actor_id, followed_actor_id)` の UNIQUE 制約 (migrations
    // 0003) で重複が来た場合は最初の行を返したい。`ON CONFLICT DO UPDATE`
    // で `updated_at` だけ動かして RETURNING する。
    sqlx::query_as!(
        FollowRow,
        r#"
        INSERT INTO follow (ap_id, follower_actor_id, followed_actor_id, state)
        VALUES ($1, $2, $3, 'pending')
        ON CONFLICT (follower_actor_id, followed_actor_id) DO UPDATE
            SET updated_at = follow.updated_at
        RETURNING id, ap_id, follower_actor_id, followed_actor_id, state, created_at, updated_at
        "#,
        ap_id,
        follower_actor_id,
        followed_actor_id,
    )
    .fetch_one(pool)
    .await
}
