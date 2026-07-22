//! Compile-time checked queries against `user_list` / `user_list_member`。
//!
//! Mastodon/Misskey 互換の「リスト」機能 (フォロー中ユーザーをグルーピング
//! した専用タイムライン)。お一人様サーバなので `owner_actor_id` は持たず、
//! 常に唯一の local actor が全リストの所有者という前提で読み書きする
//! ([`migrations/0025_user_list.sql`] 参照)。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sqlx::PgPool;
use sqlx::types::Json;

use crate::model::UserListRow;
use crate::repo::note::TimelineEntry;

pub async fn create(pool: &PgPool, title: &str) -> sqlx::Result<UserListRow> {
    sqlx::query_as!(
        UserListRow,
        r#"
        INSERT INTO user_list (title)
        VALUES ($1)
        RETURNING id, title, created_at, updated_at
        "#,
        title,
    )
    .fetch_one(pool)
    .await
}

pub async fn list_all(pool: &PgPool) -> sqlx::Result<Vec<UserListRow>> {
    sqlx::query_as!(
        UserListRow,
        r#"
        SELECT id, title, created_at, updated_at
        FROM user_list
        ORDER BY id ASC
        "#,
    )
    .fetch_all(pool)
    .await
}

pub async fn get_by_id(pool: &PgPool, id: i64) -> sqlx::Result<Option<UserListRow>> {
    sqlx::query_as!(
        UserListRow,
        r#"
        SELECT id, title, created_at, updated_at
        FROM user_list WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await
}

pub async fn rename(pool: &PgPool, id: i64, title: &str) -> sqlx::Result<Option<UserListRow>> {
    sqlx::query_as!(
        UserListRow,
        r#"
        UPDATE user_list SET title = $1, updated_at = now()
        WHERE id = $2
        RETURNING id, title, created_at, updated_at
        "#,
        title,
        id,
    )
    .fetch_optional(pool)
    .await
}

/// `ON DELETE CASCADE` で `user_list_member` の紐づく行も一緒に消える。
pub async fn delete_by_id(pool: &PgPool, id: i64) -> sqlx::Result<u64> {
    let res = sqlx::query!("DELETE FROM user_list WHERE id = $1", id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// メンバー追加時に返しうるエラー。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddMemberError {
    /// 対象リストが存在しない。
    ListNotFound,
    /// `member_actor_id` を `state = 'accepted'` で follow していない。
    /// Mastodon/Misskey と同様、フォロー済みの相手のみリストに追加できる。
    NotFollowing,
}

/// `member_actor_id` をリストに追加する。`follower_actor_id` は local actor
/// の id (= 呼び出し側が resolve 済みの値を渡す)。`follow.state = 'accepted'`
/// でなければ [`AddMemberError::NotFollowing`] を返し、DB に触らない。
///
/// 既にメンバーなら `ON CONFLICT DO NOTHING` で冪等 (エラーにしない)。
pub async fn add_member(
    pool: &PgPool,
    list_id: i64,
    follower_actor_id: i64,
    member_actor_id: i64,
) -> sqlx::Result<Result<(), AddMemberError>> {
    if get_by_id(pool, list_id).await?.is_none() {
        return Ok(Err(AddMemberError::ListNotFound));
    }
    let accepted = crate::repo::follow::get_by_pair(pool, follower_actor_id, member_actor_id)
        .await?
        .is_some_and(|f| f.state == "accepted");
    if !accepted {
        return Ok(Err(AddMemberError::NotFollowing));
    }
    sqlx::query!(
        r#"
        INSERT INTO user_list_member (list_id, member_actor_id)
        VALUES ($1, $2)
        ON CONFLICT (list_id, member_actor_id) DO NOTHING
        "#,
        list_id,
        member_actor_id,
    )
    .execute(pool)
    .await?;
    Ok(Ok(()))
}

pub async fn remove_member(pool: &PgPool, list_id: i64, member_actor_id: i64) -> sqlx::Result<u64> {
    let res = sqlx::query!(
        "DELETE FROM user_list_member WHERE list_id = $1 AND member_actor_id = $2",
        list_id,
        member_actor_id,
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// リストのメンバー actor id を昇順で列挙する (`MissUserList.userIds` 用)。
pub async fn list_member_ids(pool: &PgPool, list_id: i64) -> sqlx::Result<Vec<i64>> {
    sqlx::query_scalar!(
        r#"
        SELECT member_actor_id AS "member_actor_id!"
        FROM user_list_member
        WHERE list_id = $1
        ORDER BY member_actor_id ASC
        "#,
        list_id,
    )
    .fetch_all(pool)
    .await
}

/// リストに含まれる actor 数。
pub async fn count_members(pool: &PgPool, list_id: i64) -> sqlx::Result<i64> {
    let count: Option<i64> = sqlx::query_scalar!(
        r#"SELECT count(*) FROM user_list_member WHERE list_id = $1"#,
        list_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(count.unwrap_or(0))
}

/// リストタイムライン (= リストメンバーの投稿) を `published_at`/`id` カーソル
/// でページングして取る。`repo::note::list_home_timeline_window` と同じ列を
/// 返すが、自分自身の投稿は含めない (Mastodon/Misskey 準拠。リストは
/// 「他者をグルーピングして見る」機能であり home timeline の代替ではない)。
#[allow(clippy::too_many_arguments)]
pub async fn list_list_timeline_window(
    pool: &PgPool,
    list_id: i64,
    since_id: Option<i64>,
    until_id: Option<i64>,
    since_date: Option<DateTime<Utc>>,
    until_date: Option<DateTime<Utc>>,
    limit: i64,
) -> sqlx::Result<Vec<TimelineEntry>> {
    sqlx::query_as!(
        TimelineEntry,
        r#"
        SELECT
            n.id, n.ap_id, n.actor_id, n.content, n.language, n.in_reply_to_ap_id,
            n.in_reply_to_note_id, n.summary, n.visibility, n.sensitive,
            n.to_recipients as "to_recipients: Json<Vec<String>>",
            n.cc_recipients as "cc_recipients: Json<Vec<String>>",
            n.attachments as "attachments: Json<JsonValue>",
            n.tags as "tags: Json<JsonValue>",
            n.is_local, n.url, n.published_at, n.edited_at, n.created_at, n.updated_at,
            a.ap_id AS actor_ap_id,
            a.preferred_username AS actor_preferred_username,
            a.display_name AS actor_display_name,
            a.icon_url AS actor_icon_url
        FROM note n
        JOIN actor a ON a.id = n.actor_id
        WHERE
            n.visibility <> 'direct'
            AND n.actor_id IN (
                SELECT member_actor_id FROM user_list_member WHERE list_id = $1
            )
            AND ($2::BIGINT IS NULL OR n.id > $2)
            AND ($3::BIGINT IS NULL OR n.id < $3)
            AND ($4::TIMESTAMPTZ IS NULL OR n.published_at > $4)
            AND ($5::TIMESTAMPTZ IS NULL OR n.published_at < $5)
        ORDER BY n.id DESC
        LIMIT $6
        "#,
        list_id,
        since_id,
        until_id,
        since_date,
        until_date,
        limit,
    )
    .fetch_all(pool)
    .await
}

/// `UserListRow` に `member_count` を添えた表示用構造体
/// (`GET /api/v1/lists` / `users/lists/list` 共通で使う)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserListWithCount {
    pub list: UserListRow,
    pub member_count: i64,
}

/// 全リストを `member_count` 込みで列挙する。リスト数は手動管理前提で
/// 少数 (お一人様サーバ) なので N+1 の per-list COUNT で割り切る。
pub async fn list_all_with_counts(pool: &PgPool) -> sqlx::Result<Vec<UserListWithCount>> {
    let lists = list_all(pool).await?;
    let mut out = Vec::with_capacity(lists.len());
    for list in lists {
        let member_count = count_members(pool, list.id).await?;
        out.push(UserListWithCount { list, member_count });
    }
    Ok(out)
}
