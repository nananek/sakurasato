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
