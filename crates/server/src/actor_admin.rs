//! `sakurasato actor lock/unlock` CLI (Issue #66 / M12)。
//!
//! local actor の `manuallyApprovesFollowers` フラグを切替える。
//! 切替後は actor `Update` activity をフォロワーに配信し、相手側のキャッシュ
//! (= 鍵アカバッジ表示) を更新させる。
//!
//! # 設計判断
//!
//! - **unlock しただけでは溜まった pending Follow を auto-Accept しない**:
//!   lock 中に届いた「待ち」を unlock の事故で全部 accept してしまうと、
//!   意図しない相手に投稿が露出する事故が起きる。Mastodon でも `locked`
//!   解除と既存リクエスト承認は別操作になっており、本サーバも同じ作法に揃える。
//! - **Update の配送先**: プロフィール変更 (M7) や alias 変更 (M9) と同じ
//!   `repo::follow::list_accepted_inboxes` ── 既存フォロワー全員に新しい
//!   actor JSON を送って、相手側 UI の「鍵アカ」バッジを更新させる。

use anyhow::{Context, anyhow, bail};
use sakurasato_core::model::ActorRow;
use sakurasato_core::{Config, repo};
use serde_json::Value as JsonValue;
use tracing::{info, warn};

use crate::cli::{ActorArgs, ActorCommand};
use crate::delivery;
use crate::local_api::profile::build_update_activity;
use crate::state::AppState;

pub async fn run(config: Config, args: ActorArgs) -> anyhow::Result<()> {
    let state = AppState::from_config(config).await?;

    let next = match args.command {
        ActorCommand::Lock => true,
        ActorCommand::Unlock => false,
    };

    let (updated, queued, changed) = set_lock_state(&state, next).await?;
    if !changed {
        println!(
            "actor {ap_id} is already {label} (manually_approves_followers = {next})",
            ap_id = updated.ap_id,
            label = if next { "locked" } else { "unlocked" },
        );
        return Ok(());
    }
    println!(
        "{verb} actor {ap_id} (manually_approves_followers = {next}); Update queued for {queued} follower inbox(es)",
        verb = if next { "locked" } else { "unlocked" },
        ap_id = updated.ap_id,
    );
    Ok(())
}

/// `set_lock_state` ── CLI と local API の共通実装。
///
/// 戻り値:
/// - `updated` ── DB から読み直した actor row。
/// - `queued`  ── follower inbox に積んだ Update の本数 (no-op なら 0)。
/// - `changed` ── 値が実際に切り替わったか (= idempotent re-invoke なら false)。
pub async fn set_lock_state(
    state: &AppState,
    next: bool,
) -> anyhow::Result<(ActorRow, usize, bool)> {
    let local = local_actor(state).await?;
    if local.manually_approves_followers == next {
        return Ok((local, 0, false));
    }

    let updated = repo::actor::set_manually_approves_followers(state.pool(), local.id, next)
        .await
        .with_context(|| {
            format!(
                "set manually_approves_followers={next} for actor {}",
                local.id
            )
        })?;

    let activity = build_update_activity(&updated);
    let queued = enqueue_to_followers(state, &updated, &activity).await;
    Ok((updated, queued, true))
}

async fn local_actor(state: &AppState) -> anyhow::Result<ActorRow> {
    let host = state.config().server.host.clone();
    let user = state.config().server.user.clone();
    let row = repo::actor::get_by_username_host(state.pool(), &user, &host)
        .await
        .context("lookup local actor")?
        .ok_or_else(|| {
            anyhow!("local actor {user}@{host} not initialised; run `sakurasato-server init`")
        })?;
    if !row.is_local {
        bail!("actor {user}@{host} exists but is not local (corrupted state?)");
    }
    Ok(row)
}

async fn enqueue_to_followers(
    state: &AppState,
    local_actor: &ActorRow,
    activity: &JsonValue,
) -> usize {
    let inboxes = match repo::follow::list_accepted_inboxes(state.pool(), local_actor.id).await {
        Ok(list) => list,
        Err(err) => {
            warn!(?err, "actor lock/unlock: list_accepted_inboxes failed");
            return 0;
        }
    };
    let mut queued = 0_usize;
    for inbox in &inboxes {
        match delivery::enqueue_activity(state.pool(), local_actor.id, inbox, activity).await {
            Ok(_) => queued += 1,
            Err(err) => warn!(?err, %inbox, "actor lock/unlock: enqueue failed"),
        }
    }
    info!(
        actor = %local_actor.ap_id,
        queued,
        "Update queued for followers (lock/unlock)",
    );
    queued
}
