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

/// `followed_actor_id` を follow している (state = 'accepted') すべての
/// follower の配送先 inbox URL を列挙する。
///
/// 各 follower について `shared_inbox_url` が非 NULL ならそれ、無ければ
/// `inbox_url` を返す。**`shared_inbox_url` 優先** ── 同インスタンスに
/// 複数フォロワーが居る場合、1 回の POST で全員にまとめて配送できる
/// (Mastodon の shared inbox 慣習)。
///
/// 戻り値は **重複除外済み** ── 同 instance で複数 follower が同じ
/// `shared_inbox` を共有していても 1 件にまとめる。順序は postgres の
/// 暗黙のソートで決まり、呼び出し側はソートに依存しない。
pub async fn list_accepted_inboxes(
    pool: &PgPool,
    followed_actor_id: i64,
) -> sqlx::Result<Vec<String>> {
    sqlx::query_scalar!(
        r#"
        SELECT DISTINCT COALESCE(a.shared_inbox_url, a.inbox_url) AS "inbox_url!"
        FROM follow f
        JOIN actor a ON a.id = f.follower_actor_id
        WHERE f.followed_actor_id = $1 AND f.state = 'accepted'
        "#,
        followed_actor_id,
    )
    .fetch_all(pool)
    .await
}

/// `follower_actor_id` の **local actor** が `followed_actor_id` を `state = 'accepted'`
/// で follow しているとき、その follow 行を返す (M9 Move 自動再フォロー判定用)。
///
/// 戻り値は `(follow.id, follower_actor_id)` の組み。お一人様サーバなので
/// 該当する local follower はせいぜい 1 件だが、複数 local actor 設計に
/// 備えてリストで返す。
pub async fn list_local_following(
    pool: &PgPool,
    followed_actor_id: i64,
) -> sqlx::Result<Vec<(i64, i64)>> {
    let rows = sqlx::query!(
        r#"
        SELECT f.id AS "follow_id!", f.follower_actor_id AS "follower_actor_id!"
        FROM follow f
        JOIN actor a ON a.id = f.follower_actor_id
        WHERE f.followed_actor_id = $1
          AND f.state = 'accepted'
          AND a.is_local = true
        "#,
        followed_actor_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.follow_id, r.follower_actor_id))
        .collect())
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
