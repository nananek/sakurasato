//! Compile-time checked queries against the `block` table。
//!
//! `follow` テーブルと対称の設計だが `state` 列を持たない (= ブロックは相手の
//! 同意を要さない一方的な宣言のため `pending` 状態が存在しない)。

// block.{blocker,blocked}_actor_id naturally share a prefix; this is the
// AP terminology and aliasing would harm readability (follow.rs と同じ理由)。
#![allow(clippy::similar_names)]

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::model::{ActorRow, BlockRow};

/// `(blocker, blocked)` ペアで block 行を作る。既存行があれば何もせず
/// そのまま返す (= `ON CONFLICT ... DO UPDATE SET ap_id = block.ap_id` の
/// no-op update トリックで、conflict 時も `RETURNING` から既存行を取れる)。
///
/// `executor` は generic ── `create_block_core` が「逆方向 follow 行の削除 /
/// block 行 upsert / Block activity enqueue」を 1 トランザクションに包む
/// ために `&mut Transaction` からも呼べる必要がある
/// (`repo::follow::set_state_if_pending` と同じ設計)。
pub async fn insert<'e, E>(
    executor: E,
    ap_id: &str,
    blocker_actor_id: i64,
    blocked_actor_id: i64,
) -> sqlx::Result<BlockRow>
where
    E: sqlx::PgExecutor<'e>,
{
    sqlx::query_as!(
        BlockRow,
        r#"
        INSERT INTO block (ap_id, blocker_actor_id, blocked_actor_id)
        VALUES ($1, $2, $3)
        ON CONFLICT (blocker_actor_id, blocked_actor_id) DO UPDATE
            SET ap_id = block.ap_id
        RETURNING id, ap_id, blocker_actor_id, blocked_actor_id, created_at
        "#,
        ap_id,
        blocker_actor_id,
        blocked_actor_id,
    )
    .fetch_one(executor)
    .await
}

pub async fn get_by_pair(
    pool: &PgPool,
    blocker_actor_id: i64,
    blocked_actor_id: i64,
) -> sqlx::Result<Option<BlockRow>> {
    sqlx::query_as!(
        BlockRow,
        r#"
        SELECT id, ap_id, blocker_actor_id, blocked_actor_id, created_at
        FROM block WHERE blocker_actor_id = $1 AND blocked_actor_id = $2
        "#,
        blocker_actor_id,
        blocked_actor_id,
    )
    .fetch_optional(pool)
    .await
}

/// Undo{Block} 受信で使う。相手が振った `ap_id` から block 行を引く。
pub async fn get_by_ap_id(pool: &PgPool, ap_id: &str) -> sqlx::Result<Option<BlockRow>> {
    sqlx::query_as!(
        BlockRow,
        r#"
        SELECT id, ap_id, blocker_actor_id, blocked_actor_id, created_at
        FROM block WHERE ap_id = $1
        "#,
        ap_id,
    )
    .fetch_optional(pool)
    .await
}

/// Unblock (`delete_block_core`) / inbound `Undo{Block}` で使う。
pub async fn delete_by_pair(
    pool: &PgPool,
    blocker_actor_id: i64,
    blocked_actor_id: i64,
) -> sqlx::Result<u64> {
    let res = sqlx::query!(
        "DELETE FROM block WHERE blocker_actor_id = $1 AND blocked_actor_id = $2",
        blocker_actor_id,
        blocked_actor_id,
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

pub async fn delete_by_id(pool: &PgPool, id: i64) -> sqlx::Result<u64> {
    let res = sqlx::query!("DELETE FROM block WHERE id = $1", id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

pub async fn get_by_id(pool: &PgPool, id: i64) -> sqlx::Result<Option<BlockRow>> {
    sqlx::query_as!(
        BlockRow,
        r#"
        SELECT id, ap_id, blocker_actor_id, blocked_actor_id, created_at
        FROM block WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await
}

/// dispatch 層のホットパスガードで使う。`blocker` が `blocked` をブロック
/// しているか。
pub async fn is_blocked(
    pool: &PgPool,
    blocker_actor_id: i64,
    blocked_actor_id: i64,
) -> sqlx::Result<bool> {
    Ok(get_by_pair(pool, blocker_actor_id, blocked_actor_id)
        .await?
        .is_some())
}

/// `(block + actor)` を結合した 1 行。TUI のブロック一覧画面
/// (`GET /api/v1/blocks`) 用。`follow::FollowWithActor` と同じ形。
#[derive(Debug)]
pub struct BlockWithActor {
    pub block_id: i64,
    pub block_created_at: DateTime<Utc>,
    pub actor: ActorRow,
}

/// local actor がブロックしている actor を `block.id DESC` 順 (= 最近ブロック
/// した順) で列挙する。
pub async fn list_blocked_by_local(
    pool: &PgPool,
    local_actor_id: i64,
) -> sqlx::Result<Vec<BlockWithActor>> {
    let rows = sqlx::query!(
        r#"
        SELECT
            b.id           AS "block_id!",
            b.created_at   AS "block_created_at!",
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
            a.also_known_as as "also_known_as: sqlx::types::Json<Vec<String>>",
            a.moved_to_ap_id,
            a.is_local,
            a.actor_type,
            a.manually_approves_followers,
            a.birthday,
            a.location,
            a.lang,
            a.followed_message,
            a.fields as "fields: sqlx::types::Json<Vec<crate::model::ActorField>>",
            a.followers_count, a.following_count, a.notes_count,
            a.fetched_at,
            a.created_at   AS "actor_created_at!",
            a.updated_at   AS "actor_updated_at!"
        FROM block b
        JOIN actor a ON a.id = b.blocked_actor_id
        WHERE b.blocker_actor_id = $1
        ORDER BY b.id DESC
        "#,
        local_actor_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| BlockWithActor {
            block_id: r.block_id,
            block_created_at: r.block_created_at,
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

// DB 統合テスト (`#[sqlx::test]`) は本クレートには置かない ── `sakurasato-core`
// は tokio / sqlx testing dev-dependency を持たない設計で、既存の repo/*.rs
// (actor.rs 等) も同様に純粋なクエリ定義のみ。DB を跨ぐ検証は
// `crates/server` 側 (`AppState` 経由で呼ぶ core ロジック層のテスト、
// `sakurasato_core::MIGRATOR` を使う) に置く。
