//! Inbound `Block` / `Undo{Block}` 受信 (PR3、計画書 §5.5)。
//!
//! 受信した `Block` は「signer が我々をブロックしている」という一方的な
//! 宣言の記録に過ぎず、拒否する理由がないため常に 202 で受理する
//! (`Follow` と違い相手の同意を要さない)。以後 signer からの
//! Follow/Like/EmojiReact/mention/Announce を silent drop するホットパス
//! ガードは PR5 (`dispatch()` 冒頭) で追加する。

use anyhow::{Context, anyhow, bail};
use sakurasato_core::model::ActorRow;
use sakurasato_core::repo;
use serde_json::Value as JsonValue;
use tracing::info;

use super::DispatchError;
use super::handler::ensure_same_host;
use crate::state::AppState;

/// 受領 Block (`signer` → 我々の local actor) の処理。
///
/// 1. F4 相当の host 一致検証 (`ensure_same_host`、`handler::handle_follow`
///    と同じ関数を再利用)。
/// 2. `object` (= local actor URI) を解決、local かつ Application でないこと
///    を確認。
/// 3. **signer → local の既存フォロー関係があれば強制解除** (`follow` 行を
///    直接 DELETE。signer 発の Undo Follow ではないので Undo Follow を
///    signer に送り返す必要はない ── signer 自身が Block した以上、フォロー
///    解除の意思は明確)。
/// 4. `block` 行を `(blocker=signer.id, blocked=local.id)` で upsert。
pub(crate) async fn handle_block(
    state: &AppState,
    signer: &ActorRow,
    activity: &JsonValue,
) -> anyhow::Result<()> {
    let block_ap_id = super::extract_activity_id(activity)
        .map_err(|e| anyhow!("Block has no activity id: {e}"))?
        .to_string();
    ensure_same_host(&block_ap_id, &signer.ap_id, "Block activity id")?;

    let object_uri = super::extract_object_uri(activity)
        .map_err(|e| anyhow!("Block has no usable `object`: {e}"))?
        .to_string();

    let blocked = repo::actor::get_by_ap_id(state.pool(), &object_uri)
        .await
        .context("lookup blocked (local) actor")?
        .ok_or_else(|| anyhow!("Block target {object_uri} not found locally"))?;

    if !blocked.is_local {
        bail!(
            "Block target {} is not a local actor; refusing to accept",
            blocked.ap_id
        );
    }
    if blocked.actor_type.eq_ignore_ascii_case("Application") {
        bail!(
            "Block target {} is an Application actor; refusing to accept",
            blocked.ap_id,
        );
    }

    // signer → local (blocked) の既存フォロー関係を強制解除。signer 自身が
    // Block した意思表示なので、Undo Follow を signer に送り返す必要はない
    // (計画書 §5.5)。
    let deleted = sqlx::query!(
        "DELETE FROM follow WHERE follower_actor_id = $1 AND followed_actor_id = $2",
        signer.id,
        blocked.id,
    )
    .execute(state.pool())
    .await
    .context("delete forced-unfollow row on inbound Block")?;
    if deleted.rows_affected() > 0 {
        info!(
            follower = %signer.ap_id,
            followed = %blocked.ap_id,
            "inbound Block: removed existing follow relationship",
        );
    }

    repo::block::insert(state.pool(), &block_ap_id, signer.id, blocked.id)
        .await
        .context("upsert inbound block row")?;

    info!(
        blocker = %signer.ap_id,
        blocked = %blocked.ap_id,
        "inbound Block recorded",
    );
    Ok(())
}

/// `Undo{Block}` の処理 (`dispatch_undo` のサブ分岐から呼ばれる)。
///
/// 1. `block::get_by_ap_id` で該当行を検索。無ければ (既に消えている /
///    そもそも記録が無い) 冪等に無視する。
/// 2. `blocker_actor_id == signer.id` を確認 (なりすまし防止、
///    `UnrelatedAcceptor` と同種のガード)。
/// 3. 行を削除する。
pub(crate) async fn handle_undo_block(
    state: &AppState,
    signer: &ActorRow,
    target_ap_id: &str,
) -> Result<(), DispatchError> {
    let Some(row) = repo::block::get_by_ap_id(state.pool(), target_ap_id)
        .await
        .with_context(|| format!("lookup block row by ap_id {target_ap_id}"))
        .map_err(DispatchError::Internal)?
    else {
        info!(
            signer = %signer.ap_id,
            target = target_ap_id,
            "Undo{{Block}}: no matching block row; ignoring",
        );
        return Ok(());
    };

    if row.blocker_actor_id != signer.id {
        return Err(DispatchError::UnrelatedBlockUndo {
            signer: signer.ap_id.clone(),
            block_ap_id: target_ap_id.to_string(),
        });
    }

    repo::block::delete_by_id(state.pool(), row.id)
        .await
        .context("delete block row on inbound Undo{Block}")
        .map_err(DispatchError::Internal)?;

    info!(
        block_id = row.id,
        signer = %signer.ap_id,
        "inbound Undo{{Block}} applied; block row deleted",
    );
    Ok(())
}
