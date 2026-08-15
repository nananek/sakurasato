//! Compile-time checked queries against the `follow` table.

// follow.{follower,followed}_actor_id naturally share a prefix; this is the
// AP terminology and aliasing would harm readability.
#![allow(clippy::similar_names)]

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use sqlx::types::Json;

use crate::model::{ActorRow, FollowRow, FollowState};

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

/// `(follower_actor_id, followed_actor_id)` ペアで follow 行を引く。
///
/// M13 PR1 (Issue #79) の `GET /api/v1/actor/{id}/relationship` 用。
/// `(follower, followed)` には UNIQUE 制約があるので戻り値は 0 or 1 行。
pub async fn get_by_pair(
    pool: &PgPool,
    follower_actor_id: i64,
    followed_actor_id: i64,
) -> sqlx::Result<Option<FollowRow>> {
    sqlx::query_as!(
        FollowRow,
        r#"
        SELECT id, ap_id, follower_actor_id, followed_actor_id, state, created_at, updated_at
        FROM follow WHERE follower_actor_id = $1 AND followed_actor_id = $2
        "#,
        follower_actor_id,
        followed_actor_id,
    )
    .fetch_optional(pool)
    .await
}

/// **Issue #66 / M12 `follow-request list`**: local actor 宛 Follow を列挙する。
///
/// `state_filter`:
/// - `Some("pending")` (default) ── 承認待ちのみ
/// - `Some("all")` ── 全 state (= テスト再実行時の cleanup や、現状把握用)
/// - 他の値は `pending` と同じ扱い (= 念のための後方互換)
///
/// 返り値は [`PendingFollowRow`] — follow 行に follower actor の表示情報
/// (`preferred_username` / `host` / `display_name` / `summary` / `is_local`) を
/// JOIN したもの。順序は `follow.created_at` の昇順で固定。
pub async fn list_for_local(
    pool: &PgPool,
    state_filter: Option<&str>,
) -> sqlx::Result<Vec<PendingFollowRow>> {
    let want_all = state_filter == Some("all");
    let rows = sqlx::query!(
        r#"
        SELECT
            f.id           AS "id!",
            f.ap_id        AS "ap_id!",
            follower.ap_id AS "follower_ap_id!",
            f.state        AS "state!",
            f.created_at   AS "created_at!",
            follower.preferred_username AS "follower_preferred_username!",
            follower.host               AS "follower_host!",
            follower.display_name       AS "follower_display_name?",
            follower.summary            AS "follower_summary?",
            follower.is_local           AS "follower_is_local!"
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
        .map(|r| PendingFollowRow {
            id: r.id,
            ap_id: r.ap_id,
            follower_ap_id: r.follower_ap_id,
            state: r.state,
            created_at: r.created_at,
            follower_preferred_username: r.follower_preferred_username,
            follower_host: r.follower_host,
            follower_display_name: r.follower_display_name,
            follower_summary: r.follower_summary,
            follower_is_local: r.follower_is_local,
        })
        .collect())
}

/// `list_for_local` の 1 行分。follower は follow の宛先 (= local) とは別に
/// JOIN 済みなので、追加クエリなしで follower の表示情報が取れる。
///
/// `summary` は **HTML のまま** (`<p>…</p>` 等)。プレーン化は表示側
/// (TUI は `content::to_plain_text`、CLI は現状 summary 非表示) の責務。
/// `follower_is_local` は acct 構築 (`user` vs `user@host`) に使う。
#[derive(Debug)]
pub struct PendingFollowRow {
    pub id: i64,
    pub ap_id: String,
    pub follower_ap_id: String,
    pub state: String,
    pub created_at: DateTime<Utc>,
    pub follower_preferred_username: String,
    pub follower_host: String,
    pub follower_display_name: Option<String>,
    pub follower_summary: Option<String>,
    pub follower_is_local: bool,
}

/// `list_pending_for_local` の旧シグネチャ ── 既存呼び出し側との互換のため
/// 残す。実装は `list_for_local(None)` (= pending のみ) に委譲。
pub async fn list_pending_for_local(
    pool: &PgPool,
) -> sqlx::Result<Vec<(i64, String, String, chrono::DateTime<chrono::Utc>)>> {
    let all = list_for_local(pool, None).await?;
    Ok(all
        .into_iter()
        .map(|row| (row.id, row.ap_id, row.follower_ap_id, row.created_at))
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

/// `(follow + actor)` を結合した 1 行。
///
/// M13 PR3 (Issue #79) の `GET /api/v1/following` / `GET /api/v1/followers` で
/// 返す。`actor` は「相手側」(`list_following` なら followed、`list_followers`
/// なら follower)、`follow_state` は当該 follow 行の状態 (`pending` /
/// `accepted` / `rejected`)。`list_following` / `list_followers` は accepted
/// しか返さないので実質常に `"accepted"` だが、`pending` 含めて見るバリアントを
/// 将来足せるよう持たせておく。
///
/// `follow_id` は **`follow.id`** で、ページネーションのカーソルに使う ──
/// 自分が follow を「いつ張ったか」順 (= `follow.id` の単調列) で並べたい
/// ため、`actor.id` ではなく `follow.id` を使う。
#[derive(Debug)]
pub struct FollowWithActor {
    pub follow_id: i64,
    pub follow_state: String,
    pub follow_created_at: DateTime<Utc>,
    pub actor: ActorRow,
}

/// **M13 PR3 (Issue #79) `GET /api/v1/following`** ── ローカル actor が
/// `state = 'accepted'` で follow している actor を `follow.id DESC` 順
/// (= 最近 follow した順) で列挙する。
///
/// pending / rejected は **含めない** ── TUI の `FollowList` 画面で「フォロー
/// 中」と表示するのは accepted のみ。pending は別途 `follow-requests`
/// 系統 API で管理する設計 (`#66`)。
///
/// `before_id = None` のとき最新から `limit` 件、`Some(x)` のとき
/// `follow.id < x` の行のみ ── `note::list_home_timeline` と同じカーソル方式。
pub async fn list_following(
    pool: &PgPool,
    local_actor_id: i64,
    before_id: Option<i64>,
    limit: i64,
) -> sqlx::Result<Vec<FollowWithActor>> {
    let rows = sqlx::query!(
        r#"
        SELECT
            f.id           AS "follow_id!",
            f.state        AS "follow_state!",
            f.created_at   AS "follow_created_at!",
            a.id           AS "actor_id!",
            a.ap_id        AS "actor_ap_id!",
            a.preferred_username,
            a.host,
            a.display_name,
            a.summary,
            a.icon_url,
            a.image_url,
            a.inbox_url,
            a.shared_inbox_url,
            a.outbox_url,
            a.followers_url,
            a.following_url,
            a.public_key_id,
            a.public_key_pem,
            a.ed25519_public_key_id,
            a.ed25519_public_key_pem,
            a.also_known_as as "also_known_as: Json<Vec<String>>",
            a.moved_to_ap_id,
            a.is_local,
            a.actor_type,
            a.manually_approves_followers,
            a.birthday,
            a.location,
            a.lang,
            a.followed_message,
            a.fields as "fields: Json<Vec<crate::model::ActorField>>",
            a.followers_count, a.following_count, a.notes_count,
            a.fetched_at,
            a.created_at   AS "actor_created_at!",
            a.updated_at   AS "actor_updated_at!"
        FROM follow f
        JOIN actor a ON a.id = f.followed_actor_id
        WHERE f.follower_actor_id = $1
          AND f.state = 'accepted'
          AND ($2::BIGINT IS NULL OR f.id < $2)
        ORDER BY f.id DESC
        LIMIT $3
        "#,
        local_actor_id,
        before_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| FollowWithActor {
            follow_id: r.follow_id,
            follow_state: r.follow_state,
            follow_created_at: r.follow_created_at,
            actor: ActorRow {
                id: r.actor_id,
                ap_id: r.actor_ap_id,
                preferred_username: r.preferred_username,
                host: r.host,
                display_name: r.display_name,
                summary: r.summary,
                icon_url: r.icon_url,
                image_url: r.image_url,
                inbox_url: r.inbox_url,
                shared_inbox_url: r.shared_inbox_url,
                outbox_url: r.outbox_url,
                followers_url: r.followers_url,
                following_url: r.following_url,
                public_key_id: r.public_key_id,
                public_key_pem: r.public_key_pem,
                // local actor が混入したときも秘密鍵を API に運ばないよう明示的に
                // 落とす。`ActorRow` は `#[serde(skip)]` で守られているが、二重
                // 防御 (= API 内部のメモリ表現にも持ち込まない)。
                private_key_pem: None,
                ed25519_public_key_id: r.ed25519_public_key_id,
                ed25519_public_key_pem: r.ed25519_public_key_pem,
                ed25519_private_key_pem: None,
                also_known_as: r.also_known_as,
                moved_to_ap_id: r.moved_to_ap_id,
                is_local: r.is_local,
                actor_type: r.actor_type,
                manually_approves_followers: r.manually_approves_followers,
                birthday: r.birthday,
                location: r.location,
                lang: r.lang,
                followed_message: r.followed_message,
                fields: r.fields,
                followers_count: r.followers_count,
                following_count: r.following_count,
                notes_count: r.notes_count,
                fetched_at: r.fetched_at,
                created_at: r.actor_created_at,
                updated_at: r.actor_updated_at,
            },
        })
        .collect())
}

/// **M13 PR3 (Issue #79) `GET /api/v1/followers`** ── ローカル actor を
/// `state = 'accepted'` で follow している actor を `follow.id DESC` 順で
/// 列挙する。`list_following` と対称で、`pending`/`rejected` は除外する。
pub async fn list_followers(
    pool: &PgPool,
    local_actor_id: i64,
    before_id: Option<i64>,
    limit: i64,
) -> sqlx::Result<Vec<FollowWithActor>> {
    let rows = sqlx::query!(
        r#"
        SELECT
            f.id           AS "follow_id!",
            f.state        AS "follow_state!",
            f.created_at   AS "follow_created_at!",
            a.id           AS "actor_id!",
            a.ap_id        AS "actor_ap_id!",
            a.preferred_username,
            a.host,
            a.display_name,
            a.summary,
            a.icon_url,
            a.image_url,
            a.inbox_url,
            a.shared_inbox_url,
            a.outbox_url,
            a.followers_url,
            a.following_url,
            a.public_key_id,
            a.public_key_pem,
            a.ed25519_public_key_id,
            a.ed25519_public_key_pem,
            a.also_known_as as "also_known_as: Json<Vec<String>>",
            a.moved_to_ap_id,
            a.is_local,
            a.actor_type,
            a.manually_approves_followers,
            a.birthday,
            a.location,
            a.lang,
            a.followed_message,
            a.fields as "fields: Json<Vec<crate::model::ActorField>>",
            a.followers_count, a.following_count, a.notes_count,
            a.fetched_at,
            a.created_at   AS "actor_created_at!",
            a.updated_at   AS "actor_updated_at!"
        FROM follow f
        JOIN actor a ON a.id = f.follower_actor_id
        WHERE f.followed_actor_id = $1
          AND f.state = 'accepted'
          AND ($2::BIGINT IS NULL OR f.id < $2)
        ORDER BY f.id DESC
        LIMIT $3
        "#,
        local_actor_id,
        before_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| FollowWithActor {
            follow_id: r.follow_id,
            follow_state: r.follow_state,
            follow_created_at: r.follow_created_at,
            actor: ActorRow {
                id: r.actor_id,
                ap_id: r.actor_ap_id,
                preferred_username: r.preferred_username,
                host: r.host,
                display_name: r.display_name,
                summary: r.summary,
                icon_url: r.icon_url,
                image_url: r.image_url,
                inbox_url: r.inbox_url,
                shared_inbox_url: r.shared_inbox_url,
                outbox_url: r.outbox_url,
                followers_url: r.followers_url,
                following_url: r.following_url,
                public_key_id: r.public_key_id,
                public_key_pem: r.public_key_pem,
                private_key_pem: None,
                ed25519_public_key_id: r.ed25519_public_key_id,
                ed25519_public_key_pem: r.ed25519_public_key_pem,
                ed25519_private_key_pem: None,
                also_known_as: r.also_known_as,
                moved_to_ap_id: r.moved_to_ap_id,
                is_local: r.is_local,
                actor_type: r.actor_type,
                manually_approves_followers: r.manually_approves_followers,
                birthday: r.birthday,
                location: r.location,
                lang: r.lang,
                followed_message: r.followed_message,
                fields: r.fields,
                followers_count: r.followers_count,
                following_count: r.following_count,
                notes_count: r.notes_count,
                fetched_at: r.fetched_at,
                created_at: r.actor_created_at,
                updated_at: r.actor_updated_at,
            },
        })
        .collect())
}

/// **M14 #158** ── `local_actor_id` が `state = 'accepted'` で follow している
/// 件数 (= `MissUser` の `followingCount` 計算用)。
///
/// `list_following` を取得して `len()` を返すより専用 COUNT を持つ方が安く済む ──
/// お一人様サーバ前提でも following が 100+ になれば差が出る
/// ため、Misskey クライアントが `/api/i` を polling 的に叩く想定で軽量化。
/// `Option<i64>` は sqlx の `count(*)` 推論で常に Some を返すため
/// `unwrap_or(0)` で受ける ([`crate::repo::note::count_local`] と同じ流儀)。
pub async fn count_following(pool: &PgPool, local_actor_id: i64) -> sqlx::Result<i64> {
    let count: Option<i64> = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM follow
        WHERE follower_actor_id = $1 AND state = 'accepted'
        "#,
        local_actor_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(count.unwrap_or(0))
}

/// **M14 #158** ── `local_actor_id` を `state = 'accepted'` で follow して
/// いる件数 (= `MissUser` の `followersCount` 計算用)。[`count_following`] と対称。
pub async fn count_followers(pool: &PgPool, local_actor_id: i64) -> sqlx::Result<i64> {
    let count: Option<i64> = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM follow
        WHERE followed_actor_id = $1 AND state = 'accepted'
        "#,
        local_actor_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(count.unwrap_or(0))
}
