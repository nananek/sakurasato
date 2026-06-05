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

/// `set_lock_state` の結果。`changed = false` のとき `queued = 0`。
/// インフラ失敗 (DB / enqueue) は `anyhow::Result` 側で表現する。
#[derive(Debug)]
pub struct LockOutcome {
    pub updated: ActorRow,
    pub queued: usize,
    pub changed: bool,
    /// **PR #80 round-2 #4**: `list_accepted_inboxes` に失敗した場合、エラー
    /// を握り潰して "queued=0 / 成功" と報告すると管理者は鍵アカ状態の連合
    /// 通知が届いた前提で動いてしまう。代わりに「enqueue 経路で何件 warn
    /// だけ残してスキップしたか」を別フィールドで返し、呼び出し側
    /// (CLI / local API) で警告表示する。
    pub enqueue_failures: usize,
}

pub async fn run(config: Config, args: ActorArgs) -> anyhow::Result<()> {
    let state = AppState::from_config(config).await?;

    let next = match args.command {
        ActorCommand::Lock => true,
        ActorCommand::Unlock => false,
    };

    let outcome = set_lock_state(&state, next).await?;
    if !outcome.changed {
        println!(
            "actor {ap_id} is already {label} (manually_approves_followers = {next})",
            ap_id = outcome.updated.ap_id,
            label = if next { "locked" } else { "unlocked" },
        );
        return Ok(());
    }
    println!(
        "{verb} actor {ap_id} (manually_approves_followers = {next}); Update queued for {queued} follower inbox(es)",
        verb = if next { "locked" } else { "unlocked" },
        ap_id = outcome.updated.ap_id,
        queued = outcome.queued,
    );
    if outcome.enqueue_failures > 0 {
        // **#4 round-2 fix**: 黙って 0 件配送と報告すると管理者が誤解する。
        // 失敗件数を stderr で明示しておく (= CLI 出力でも目に入る)。
        eprintln!(
            "WARNING: failed to enqueue Update for {n} follower inbox(es); see server logs",
            n = outcome.enqueue_failures,
        );
    }
    Ok(())
}

/// `set_lock_state` ── CLI と local API の共通実装。
///
/// **PR #80 round-2 #4 修正**:
/// 旧実装は `list_accepted_inboxes` 失敗を `warn!` + `queued=0` で吸収して
/// しまい、管理者が「Update が全 follower に届いた」と誤解する経路があった。
/// 修正後は:
/// - `list_accepted_inboxes` の失敗は **`anyhow::Error` でエスカレート**
///   する (= CLI / HTTP 層から失敗が見える)。
/// - 個別 `enqueue_activity` の失敗は warn し続け、件数を `enqueue_failures`
///   に詰めて呼び出し側に通知する (1 件失敗で全体を bail すると、たまたま
///   inbox URL が壊れている follower が居るだけで lock 切替自体ができなく
///   なるため)。
pub async fn set_lock_state(state: &AppState, next: bool) -> anyhow::Result<LockOutcome> {
    let local = local_actor(state).await?;
    if local.manually_approves_followers == next {
        return Ok(LockOutcome {
            updated: local,
            queued: 0,
            changed: false,
            enqueue_failures: 0,
        });
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
    let (queued, enqueue_failures) = enqueue_to_followers(state, &updated, &activity).await?;
    Ok(LockOutcome {
        updated,
        queued,
        changed: true,
        enqueue_failures,
    })
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
) -> anyhow::Result<(usize, usize)> {
    let inboxes = repo::follow::list_accepted_inboxes(state.pool(), local_actor.id)
        .await
        .context("list accepted follower inboxes")?;
    let mut queued = 0_usize;
    let mut failures = 0_usize;
    for inbox in &inboxes {
        match delivery::enqueue_activity(state.pool(), local_actor.id, inbox, activity).await {
            Ok(_) => queued += 1,
            Err(err) => {
                failures += 1;
                warn!(?err, %inbox, "actor lock/unlock: enqueue failed");
            }
        }
    }
    if queued > 0 {
        state.wake_delivery();
    }
    info!(
        actor = %local_actor.ap_id,
        queued,
        failures,
        "Update queued for followers (lock/unlock)",
    );
    Ok((queued, failures))
}
