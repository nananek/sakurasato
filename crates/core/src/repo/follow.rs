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

/// `follow.id` で 1 行引く (`follow-request approve/reject` CLI 用)。
///
/// `executor` を generic にしているのは、approve/reject のトランザクション
/// 内で「CAS 後に現状を再 fetch する」用途のため (= 同じ tx を共有する)。
pub async fn get_by_id<'e, E>(executor: E, id: i64) -> sqlx::Result<Option<FollowRow>>
where
    E: sqlx::PgExecutor<'e>,
{
    sqlx::query_as!(
        FollowRow,
        r#"
        SELECT id, ap_id, follower_actor_id, followed_actor_id, state, created_at, updated_at
        FROM follow WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(executor)
    .await
}

/// **Issue #66 / M12 `follow-request list`**: local actor 宛 Follow を列挙する。
///
/// `state_filter`:
/// - `Some("pending")` (default) ── 承認待ちのみ
/// - `Some("all")` ── 全 state (= テスト再実行時の cleanup や、現状把握用)
/// - 他の値は `pending` と同じ扱い (= 念のための後方互換)
///
/// 返り値は `(follow.id, follow.ap_id, follower.ap_id, state, created_at)`。
/// 順序は `follow.created_at` の昇順で固定。
pub async fn list_for_local(
    pool: &PgPool,
    state_filter: Option<&str>,
) -> sqlx::Result<Vec<(i64, String, String, String, chrono::DateTime<chrono::Utc>)>> {
    let want_all = state_filter == Some("all");
    let rows = sqlx::query!(
        r#"
        SELECT
            f.id           AS "id!",
            f.ap_id        AS "ap_id!",
            follower.ap_id AS "follower_ap_id!",
            f.state        AS "state!",
            f.created_at   AS "created_at!"
        FROM follow f
        JOIN actor follower ON follower.id = f.follower_actor_id
        JOIN actor followed ON followed.id = f.followed_actor_id
        WHERE followed.is_local = TRUE
          AND ($1::BOOLEAN = TRUE OR f.state = 'pending')
        ORDER BY f.created_at ASC
        "#,
        want_all,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.id, r.ap_id, r.follower_ap_id, r.state, r.created_at))
        .collect())
}

/// `list_pending_for_local` の旧シグネチャ ── 既存呼び出し側との互換のため
/// 残す。実装は `list_for_local(None)` (= pending のみ) に委譲。
pub async fn list_pending_for_local(
    pool: &PgPool,
) -> sqlx::Result<Vec<(i64, String, String, chrono::DateTime<chrono::Utc>)>> {
    let all = list_for_local(pool, None).await?;
    Ok(all
        .into_iter()
        .map(|(id, ap_id, follower_ap_id, _state, created_at)| {
            (id, ap_id, follower_ap_id, created_at)
        })
        .collect())
}

/// `follow.id` で 1 行ハード削除する (PR #80 round-2 #6: テスト cleanup 用)。
/// Undo Follow ハンドラ未実装の現状、accepted のままの古いフォロワー行を
/// 管理者が明示的に消すための補助 API。戻り値は影響行数 (0 か 1)。
pub async fn delete_by_id(pool: &PgPool, id: i64) -> sqlx::Result<u64> {
    let res = sqlx::query!("DELETE FROM follow WHERE id = $1", id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
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

/// **PR #80 round-2 (#1 atomic CAS)**: `state = 'pending'` の行だけ
/// 指定 `new_state` (= Accepted / Rejected) に遷移する。影響行数 (= 0 or 1)
/// を返す ── 0 なら呼び出し側が「他のセッションが先に倒した」と判断し、
/// Accept/Reject の重複 enqueue を回避できる。
///
/// `executor` は generic で、`set_state_if_pending` + `enqueue` を 1 つの
/// `Transaction` で囲んで「state 遷移と Accept enqueue を不可分にする」
/// 用途を想定する (= `follow_request::approve_or_reject` の round-2 修正)。
pub async fn set_state_if_pending<'e, E>(
    executor: E,
    id: i64,
    new_state: FollowState,
) -> sqlx::Result<u64>
where
    E: sqlx::PgExecutor<'e>,
{
    let res = sqlx::query!(
        "UPDATE follow SET state = $1, updated_at = now() \
         WHERE id = $2 AND state = 'pending'",
        new_state.as_str(),
        id,
    )
    .execute(executor)
    .await?;
    Ok(res.rows_affected())
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
///
/// **PR #80 round-2 (rejected → re-follow 復活):**
/// `(follower, followed)` UNIQUE 制約上 ON CONFLICT で既存行が返るが、その
/// ときの分岐は以下:
///
/// - 既存 `state = 'accepted'` → そのまま (idempotent retry。Accept 再送は
///   `handle_follow` の Accepted ブランチで対応)。
/// - 既存 `state = 'pending'`  → そのまま (二重配送による pending 重複)。
/// - 既存 `state = 'rejected'` → `ap_id` を新しい値に書き換え、state を
///   `pending` にリセットする (= リモートが Unfollow → 再 Follow した時に
///   永久ブロックされないように)。同時に `updated_at` も動かす。
///
/// `EXCLUDED.ap_id` は INSERT を試みた行の `ap_id` (= 新しく届いた Follow の
/// activity id)。これにより `ap_id` を「最新の Follow と一致する値」に保つ。
pub async fn upsert_pending(
    pool: &PgPool,
    ap_id: &str,
    follower_actor_id: i64,
    followed_actor_id: i64,
) -> sqlx::Result<FollowRow> {
    if let Some(existing) = get_by_ap_id(pool, ap_id).await? {
        return Ok(existing);
    }
    sqlx::query_as!(
        FollowRow,
        r#"
        INSERT INTO follow (ap_id, follower_actor_id, followed_actor_id, state)
        VALUES ($1, $2, $3, 'pending')
        ON CONFLICT (follower_actor_id, followed_actor_id) DO UPDATE
            SET ap_id = CASE WHEN follow.state = 'rejected' THEN EXCLUDED.ap_id ELSE follow.ap_id END,
                state = CASE WHEN follow.state = 'rejected' THEN 'pending'      ELSE follow.state END,
                updated_at = CASE WHEN follow.state = 'rejected' THEN now()     ELSE follow.updated_at END
        RETURNING id, ap_id, follower_actor_id, followed_actor_id, state, created_at, updated_at
        "#,
        ap_id,
        follower_actor_id,
        followed_actor_id,
    )
    .fetch_one(pool)
    .await
}
