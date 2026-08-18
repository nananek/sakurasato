//! Block / Unblock の core ロジック (`sakurasato-server block` CLI と
//! `POST|DELETE /api/v1/block*` local API の共有実装)。
//!
//! tmp/plan-block-unfollow-domain-block.md §5.4 準拠、`follow.rs` と対称の
//! 構成。target 解決 (`acct`/`actor_uri`/`actor_id` の 3 択) は
//! [`crate::follow::resolve_target_actor`] / [`crate::follow::resolve_local_actor`]
//! をそのまま再利用する (計画書 §0 / §5.4 で明示された既存実装の再利用ポイント)。
//!
//! # ブロック実行時の双方向フォロー強制解除
//!
//! - **local → target** に既存 `pending`/`accepted` 行があれば、既存の
//!   [`crate::follow::delete_follow_core`] をそのまま呼ぶ (Undo Follow 送出 +
//!   行削除)。これが計画書 §0 で触れた「既存 Unfollow 実装の再利用ポイント」。
//! - **target → local** に既存 `pending`/`accepted` 行があれば、`Reject` は
//!   送出せず単純に行を削除するだけに留める (計画書 §10 確定事項 #1:
//!   ブロック済み actor からの Follow はサイレントドロップ方針に合わせ、
//!   ブロック実行時点で既に存在する逆方向フォローも黙って削除する)。
//!
//! `delete_follow_core` は自前でトランザクションを begin/commit するため、
//! ここでは「そのまま呼ぶ」を文字通り実施し、block 行の作成 + Block activity
//! の enqueue (+ 逆方向 follow 行削除) は別の 1 トランザクションにまとめる。

use anyhow::Context;
use sakurasato_core::model::{ActorRow, BlockRow};
use sakurasato_core::repo;
use serde_json::{Value as JsonValue, json};
use thiserror::Error;
use tracing::info;

use crate::cli::{BlockArgs, BlockCommand, BlockCreateArgs, BlockIdArgs};
use crate::delivery;
use crate::follow::{self, FollowError, FollowTarget};
use crate::state::AppState;

/// `create_block_core` / `delete_block_core` の終端エラー。`FollowError` と
/// 同じ分類を踏襲する (HTTP マッピングも `local_api/follow.rs::map_follow_error`
/// と対称に `local_api/block.rs` で行う)。
#[derive(Debug, Error)]
pub enum BlockError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    BadGateway(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Unavailable(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Forbidden(String),
    #[error(transparent)]
    Internal(anyhow::Error),
}

impl From<sqlx::Error> for BlockError {
    fn from(e: sqlx::Error) -> Self {
        Self::Internal(e.into())
    }
}

/// target 解決 / 強制 Unfollow (`delete_follow_core` 再利用) から来る
/// `FollowError` を同名バリアントへそのまま写す。
impl From<FollowError> for BlockError {
    fn from(e: FollowError) -> Self {
        match e {
            FollowError::BadRequest(m) => Self::BadRequest(m),
            FollowError::BadGateway(m) => Self::BadGateway(m),
            FollowError::NotFound(m) => Self::NotFound(m),
            FollowError::Unavailable(m) => Self::Unavailable(m),
            FollowError::Conflict(m) => Self::Conflict(m),
            FollowError::Forbidden(m) => Self::Forbidden(m),
            FollowError::Internal(e) => Self::Internal(e),
        }
    }
}

#[derive(Debug, Clone)]
pub struct BlockOutcome {
    pub block: BlockRow,
    pub target: ActorRow,
    pub queue_id: i64,
    pub inbox_url: String,
}

#[derive(Debug, Clone)]
pub struct UnblockOutcome {
    pub block_id: i64,
    pub target_ap_id: String,
    pub queue_id: i64,
    pub inbox_url: String,
}

/// **PR2 core**: 指定 target をブロックする。
///
/// 1. local actor 解決、target actor 解決 (`FollowTarget` 解決ロジックを共有)。
/// 2. 自分自身のブロックは `Conflict` で拒否。
/// 3. 双方向フォロー強制解除 (モジュール doc 参照)。
/// 4. 決定論的 `block-cli-{blocker}-{blocked}` で block 行を upsert + Block
///    activity を `delivery_queue` に enqueue。3b/4 は 1 トランザクションに
///    包み、`commit()` 後に `wake_delivery()`。
pub async fn create_block_core(
    state: &AppState,
    target: FollowTarget,
) -> Result<BlockOutcome, BlockError> {
    let local = follow::resolve_local_actor(state).await?;
    let target_actor = follow::resolve_target_actor(state, target).await?;

    if target_actor.id == local.id {
        return Err(BlockError::Conflict(format!(
            "refusing to block our own local actor {:?}",
            local.ap_id,
        )));
    }

    // local → target: 既存 pending/accepted 行があれば delete_follow_core を
    // そのまま呼ぶ (Undo Follow 送出 + 行削除)。
    if let Some(row) = repo::follow::get_by_pair(state.pool(), local.id, target_actor.id).await?
        && matches!(row.state.as_str(), "pending" | "accepted")
    {
        follow::delete_follow_core(state, row.id).await?;
    }

    let block_ap_id = build_block_ap_id(state, &local, target_actor.id);
    let inbox = target_actor
        .shared_inbox_url
        .as_deref()
        .unwrap_or(&target_actor.inbox_url)
        .to_string();
    let activity = build_block_activity(&block_ap_id, &local.ap_id, &target_actor.ap_id);

    let mut tx = state
        .pool()
        .begin()
        .await
        .map_err(|e| BlockError::Internal(anyhow::Error::new(e).context("begin transaction")))?;
    // target → local: Reject は送出せず単純に行を削除するだけに留める
    // (計画書 §10 確定事項 #1)。
    sqlx::query!(
        "DELETE FROM follow WHERE follower_actor_id = $1 AND followed_actor_id = $2",
        target_actor.id,
        local.id,
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| {
        BlockError::Internal(anyhow::Error::new(e).context("delete reverse follow row in tx"))
    })?;
    let block_row = repo::block::insert(&mut *tx, &block_ap_id, local.id, target_actor.id)
        .await
        .map_err(|e| BlockError::Internal(anyhow::Error::new(e).context("insert block row in tx")))?;
    let queued = delivery::enqueue_activity(&mut *tx, local.id, &inbox, &activity)
        .await
        .map_err(|e| BlockError::Internal(e.context(format!("enqueue Block to {inbox}"))))?;
    tx.commit().await.map_err(|e| {
        BlockError::Internal(anyhow::Error::new(e).context("commit block transaction"))
    })?;
    // commit 後に wake する (delete_follow_core と同じ理由: commit 前は他
    // コネクションから見えず、早く起こしても pick_due が空振りする)。
    state.wake_delivery();

    info!(
        block_id = block_row.id,
        target = %target_actor.ap_id,
        queue_id = queued.id,
        "Block queued; reverse follow (if any) removed",
    );
    Ok(BlockOutcome {
        block: block_row,
        target: target_actor,
        queue_id: queued.id,
        inbox_url: inbox,
    })
}

/// **PR2 core**: 既存 block 行を取り消す (outbound `Undo{Block}` + 行削除)。
///
/// 認可: `blocker_actor_id` が local actor の id と一致する行のみ削除可能。
/// フォロー関係は自動復活させない (Misskey/Mastodon とも復活させない方針)。
pub async fn delete_block_core(
    state: &AppState,
    block_id: i64,
) -> Result<UnblockOutcome, BlockError> {
    let local = follow::resolve_local_actor(state).await?;
    let row = repo::block::get_by_id(state.pool(), block_id)
        .await?
        .ok_or_else(|| BlockError::NotFound(format!("no block row with id={block_id}")))?;
    if row.blocker_actor_id != local.id {
        return Err(BlockError::Forbidden(format!(
            "block id={} is not owned by the local actor (blocker_actor_id={})",
            row.id, row.blocker_actor_id,
        )));
    }
    let target = repo::actor::get_by_id(state.pool(), row.blocked_actor_id)
        .await?
        .ok_or_else(|| {
            BlockError::Internal(anyhow::anyhow!(
                "block row id={} references missing target actor",
                row.id,
            ))
        })?;

    let undo_ap_id = format!(
        "https://{host}/users/{user}/activities/undo-block-{block_id}",
        host = state.config().server.host,
        user = local.preferred_username,
    );
    let inbox = target
        .shared_inbox_url
        .as_deref()
        .unwrap_or(&target.inbox_url)
        .to_string();
    let activity = build_undo_block_activity(&undo_ap_id, &local.ap_id, &row, &target.ap_id);

    let mut tx = state
        .pool()
        .begin()
        .await
        .map_err(|e| BlockError::Internal(anyhow::Error::new(e).context("begin transaction")))?;
    let queued = delivery::enqueue_activity(&mut *tx, local.id, &inbox, &activity)
        .await
        .map_err(|e| BlockError::Internal(e.context(format!("enqueue Undo Block to {inbox}"))))?;
    sqlx::query!("DELETE FROM block WHERE id = $1", row.id)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            BlockError::Internal(anyhow::Error::new(e).context("delete block row in tx"))
        })?;
    tx.commit().await.map_err(|e| {
        BlockError::Internal(anyhow::Error::new(e).context("commit unblock transaction"))
    })?;
    state.wake_delivery();

    info!(
        block_id = row.id,
        target = %target.ap_id,
        queue_id = queued.id,
        "Undo Block queued; block row deleted",
    );
    Ok(UnblockOutcome {
        block_id: row.id,
        target_ap_id: target.ap_id,
        queue_id: queued.id,
        inbox_url: inbox,
    })
}

/// CLI `sakurasato-server block <create|unblock|list>` の入口。
pub async fn run(config: sakurasato_core::Config, args: BlockArgs) -> anyhow::Result<()> {
    let state = AppState::from_config(config).await?;
    match args.command {
        BlockCommand::Create(a) => run_create(&state, a).await,
        BlockCommand::Unblock(a) => run_unblock(&state, a).await,
        BlockCommand::List => run_list(&state).await,
    }
}

async fn run_create(state: &AppState, args: BlockCreateArgs) -> anyhow::Result<()> {
    let target = if let Some(uri) = args.actor_uri.as_deref() {
        FollowTarget::ActorUri(uri.to_string())
    } else {
        FollowTarget::Acct(args.acct.unwrap_or_default())
    };
    let outcome = create_block_core(state, target)
        .await
        .map_err(|e| match e {
            BlockError::Internal(err) => err,
            other => anyhow::anyhow!("{other}"),
        })?;
    // CLI は daemon とは別プロセスなので wake_delivery だけでは worker が
    // 起床しない (follow.rs::run と同じ理由)。自プロセスで即 flush する。
    delivery::flush_due_now(state).await;
    println!(
        "queued Block against {target}: block_id={block_id} delivery_queue_id={queue_id} inbox={inbox}",
        target = outcome.target.ap_id,
        block_id = outcome.block.id,
        queue_id = outcome.queue_id,
        inbox = outcome.inbox_url,
    );
    Ok(())
}

async fn run_unblock(state: &AppState, args: BlockIdArgs) -> anyhow::Result<()> {
    let outcome = delete_block_core(state, args.id)
        .await
        .map_err(|e| match e {
            BlockError::Internal(err) => err,
            other => anyhow::anyhow!("{other}"),
        })?;
    delivery::flush_due_now(state).await;
    println!(
        "queued Undo Block against {target}: block_id={block_id} delivery_queue_id={queue_id} inbox={inbox}",
        target = outcome.target_ap_id,
        block_id = outcome.block_id,
        queue_id = outcome.queue_id,
        inbox = outcome.inbox_url,
    );
    Ok(())
}

async fn run_list(state: &AppState) -> anyhow::Result<()> {
    let local = follow::resolve_local_actor(state)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let rows = repo::block::list_blocked_by_local(state.pool(), local.id)
        .await
        .context("list blocked actors")?;
    if rows.is_empty() {
        println!("no blocked actors");
        return Ok(());
    }
    for row in rows {
        println!(
            "id={id} actor={ap_id} blocked_at={created_at}",
            id = row.block_id,
            ap_id = row.actor.ap_id,
            created_at = row.block_created_at,
        );
    }
    Ok(())
}

fn build_block_ap_id(state: &AppState, local: &ActorRow, target_actor_id: i64) -> String {
    format!(
        "https://{host}/users/{user}/activities/block-cli-{blocker}-{blocked}",
        host = state.config().server.host,
        user = local.preferred_username,
        blocker = local.id,
        blocked = target_actor_id,
    )
}

fn build_block_activity(ap_id: &str, actor: &str, object: &str) -> JsonValue {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": ap_id,
        "type": "Block",
        "actor": actor,
        "object": object,
    })
}

/// `Undo{Block}` activity を組み立てる。`build_undo_follow_activity` と同じく
/// `object` に元 Block の inline 形式を埋め込む。
fn build_undo_block_activity(
    undo_ap_id: &str,
    local_ap_id: &str,
    block_row: &BlockRow,
    target_ap_id: &str,
) -> JsonValue {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": undo_ap_id,
        "type": "Undo",
        "actor": local_ap_id,
        "object": {
            "id": block_row.ap_id,
            "type": "Block",
            "actor": local_ap_id,
            "object": target_ap_id,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn fake_block_row(id: i64) -> BlockRow {
        BlockRow {
            id,
            ap_id: format!("https://x/users/me/activities/block-cli-1-{id}"),
            blocker_actor_id: 1,
            blocked_actor_id: id,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn build_block_activity_has_required_fields() {
        let a = build_block_activity(
            "https://x/users/me/activities/block-cli-1-2",
            "https://x/users/me",
            "https://y/users/bob",
        );
        assert_eq!(a["type"], "Block");
        assert_eq!(a["id"], "https://x/users/me/activities/block-cli-1-2");
        assert_eq!(a["actor"], "https://x/users/me");
        assert_eq!(a["object"], "https://y/users/bob");
        assert_eq!(a["@context"], "https://www.w3.org/ns/activitystreams");
    }

    #[test]
    fn build_undo_block_activity_wraps_inline_block() {
        let row = fake_block_row(42);
        let undo = build_undo_block_activity(
            "https://x/users/me/activities/undo-block-42",
            "https://x/users/me",
            &row,
            "https://y/users/bob",
        );
        assert_eq!(undo["type"], "Undo");
        assert_eq!(undo["id"], "https://x/users/me/activities/undo-block-42");
        assert_eq!(undo["actor"], "https://x/users/me");
        assert_eq!(undo["object"]["type"], "Block");
        assert_eq!(undo["object"]["id"], row.ap_id);
        assert_eq!(undo["object"]["actor"], "https://x/users/me");
        assert_eq!(undo["object"]["object"], "https://y/users/bob");
        assert_eq!(undo["@context"], "https://www.w3.org/ns/activitystreams");
    }
}
