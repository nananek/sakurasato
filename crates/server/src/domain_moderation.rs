//! ドメインモデレーション (silence/suspend) の core ロジック
//! (`sakurasato-server domain` CLI と `/api/v1/domains*` local API の共有実装)。
//!
//! tmp/plan-block-unfollow-domain-block.md §6.4 準拠。
//!
//! - **silence**: `domain_moderation` 行を `severity=silence` で upsert するのみ。
//!   既存フォロー関係には一切触れない (計画書 §10 確定事項 #3)。効果は
//!   `dispatch/handler.rs::handle_follow` 側のガード (PR5) で発揮される。
//! - **suspend**: `domain_moderation` 行を `severity=suspend` で upsert した後、
//!   そのドメインに属する **双方向のフォロー関係を全部強制解除** する。
//!   **`Undo Follow` / `Reject` は一切配送せず、DB 上の関係解消のみ行う**
//!   (計画書 §10 確定事項 #2)。単一ユーザーサーバでは 1 ドメインあたりの
//!   フォロー件数は少数に留まる想定のため、`follow.follower_actor_id` /
//!   `followed_actor_id` を host でまとめて絞る 2 本の一括 DELETE で行う
//!   (行ごとの逐次処理・進捗表示は行わない)。

use sakurasato_core::model::{DomainModerationRow, DomainSeverity};
use sakurasato_core::repo;
use sakurasato_core::repo::domain_moderation::DomainStats;
use sakurasato_core::repo::follow::FollowWithActor;
use thiserror::Error;
use tracing::info;

use crate::cli::{DomainArgs, DomainCommand, DomainHostArgs, DomainModerateArgs};
use crate::follow;
use crate::state::AppState;

#[derive(Debug, Error)]
pub enum DomainModerationError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Unavailable(String),
    #[error(transparent)]
    Internal(anyhow::Error),
}

impl From<sqlx::Error> for DomainModerationError {
    fn from(e: sqlx::Error) -> Self {
        Self::Internal(e.into())
    }
}

/// `suspend_core` の結果。強制解除した follow 行数 (following + followers 合算)
/// を返す ── CLI / API 双方が「N 件のフォロー関係を解除した」と報告できるように。
#[derive(Debug, Clone)]
pub struct SuspendOutcome {
    pub row: DomainModerationRow,
    pub forced_unfollow_count: u64,
}

/// `GET /api/v1/domains/{host}` / CLI `domain info <host>` 用の詳細。
#[derive(Debug)]
pub struct DomainDetail {
    pub host: String,
    pub moderation: Option<DomainModerationRow>,
    pub stats: DomainStats,
    pub following: Vec<FollowWithActor>,
    pub followers: Vec<FollowWithActor>,
}

fn normalize_host(raw: &str) -> Result<String, DomainModerationError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(DomainModerationError::BadRequest(
            "host must not be empty".into(),
        ));
    }
    Ok(trimmed.to_ascii_lowercase())
}

async fn resolve_local_actor_id(state: &AppState) -> Result<i64, DomainModerationError> {
    let local = follow::resolve_local_actor(state)
        .await
        .map_err(|e| DomainModerationError::Unavailable(format!("{e}")))?;
    Ok(local.id)
}

/// 配信停止を設定する。既存フォロー関係には影響しない (計画書 §10 確定事項 #3)。
pub async fn silence_core(
    state: &AppState,
    host: &str,
    reason: Option<String>,
) -> Result<DomainModerationRow, DomainModerationError> {
    let host = normalize_host(host)?;
    let row = repo::domain_moderation::upsert(
        state.pool(),
        &host,
        DomainSeverity::Silence.as_str(),
        reason.as_deref(),
    )
    .await?;
    info!(host = %row.host, "domain silenced");
    Ok(row)
}

/// 完全ブロックを実行する。`domain_moderation` 行を suspend で upsert した後、
/// そのドメインに属する双方向フォロー行を全部強制解除する (配送は一切しない、
/// モジュール doc 参照)。
pub async fn suspend_core(
    state: &AppState,
    host: &str,
    reason: Option<String>,
) -> Result<SuspendOutcome, DomainModerationError> {
    let host = normalize_host(host)?;
    let local_id = resolve_local_actor_id(state).await?;

    let row = repo::domain_moderation::upsert(
        state.pool(),
        &host,
        DomainSeverity::Suspend.as_str(),
        reason.as_deref(),
    )
    .await?;

    // local → host: 自分が host 内の相手を follow している行を一括削除。
    let following_deleted = sqlx::query!(
        r#"
        DELETE FROM follow
        WHERE follower_actor_id = $1
          AND followed_actor_id IN (SELECT id FROM actor WHERE host = $2)
        "#,
        local_id,
        host,
    )
    .execute(state.pool())
    .await?
    .rows_affected();

    // host → local: host 内の相手が自分を follow している行を一括削除。
    let followers_deleted = sqlx::query!(
        r#"
        DELETE FROM follow
        WHERE followed_actor_id = $1
          AND follower_actor_id IN (SELECT id FROM actor WHERE host = $2)
        "#,
        local_id,
        host,
    )
    .execute(state.pool())
    .await?
    .rows_affected();

    let forced_unfollow_count = following_deleted + followers_deleted;
    info!(
        host = %row.host,
        forced_unfollow_count,
        "domain suspended; follow relationships force-removed (no Undo Follow/Reject sent)",
    );
    Ok(SuspendOutcome {
        row,
        forced_unfollow_count,
    })
}

/// 措置解除。行が無ければ [`DomainModerationError::NotFound`]。
pub async fn unset_core(state: &AppState, host: &str) -> Result<(), DomainModerationError> {
    let host = normalize_host(host)?;
    let affected = repo::domain_moderation::delete_by_host(state.pool(), &host).await?;
    if affected == 0 {
        return Err(DomainModerationError::NotFound(format!(
            "no moderation set for host {host:?}"
        )));
    }
    info!(host = %host, "domain moderation unset");
    Ok(())
}

pub async fn list_core(
    state: &AppState,
) -> Result<Vec<DomainModerationRow>, DomainModerationError> {
    Ok(repo::domain_moderation::list(state.pool()).await?)
}

/// `host` の統計 + moderation state + フォロー一覧をまとめて返す。
pub async fn detail_core(
    state: &AppState,
    host: &str,
) -> Result<DomainDetail, DomainModerationError> {
    let host = normalize_host(host)?;
    let local_id = resolve_local_actor_id(state).await?;
    let moderation = repo::domain_moderation::get_by_host(state.pool(), &host).await?;
    let stats = repo::domain_moderation::domain_stats(state.pool(), local_id, &host).await?;
    let following =
        repo::domain_moderation::list_following_in_domain(state.pool(), local_id, &host).await?;
    let followers =
        repo::domain_moderation::list_followers_in_domain(state.pool(), local_id, &host).await?;
    Ok(DomainDetail {
        host,
        moderation,
        stats,
        following,
        followers,
    })
}

/// CLI `sakurasato-server domain <list|info|silence|suspend|unset>` の入口。
pub async fn run(config: sakurasato_core::Config, args: DomainArgs) -> anyhow::Result<()> {
    let state = AppState::from_config(config).await?;
    match args.command {
        DomainCommand::List => run_list(&state).await,
        DomainCommand::Info(a) => run_info(&state, a).await,
        DomainCommand::Silence(a) => run_silence(&state, a).await,
        DomainCommand::Suspend(a) => run_suspend(&state, a).await,
        DomainCommand::Unset(a) => run_unset(&state, a).await,
    }
}

fn to_anyhow(e: DomainModerationError) -> anyhow::Error {
    match e {
        DomainModerationError::Internal(err) => err,
        other => anyhow::anyhow!("{other}"),
    }
}

async fn run_list(state: &AppState) -> anyhow::Result<()> {
    let rows = list_core(state).await.map_err(to_anyhow)?;
    if rows.is_empty() {
        println!("no domain moderation set");
        return Ok(());
    }
    for row in rows {
        println!(
            "host={host} severity={severity} reason={reason:?} updated_at={updated_at}",
            host = row.host,
            severity = row.severity,
            reason = row.reason,
            updated_at = row.updated_at,
        );
    }
    Ok(())
}

async fn run_info(state: &AppState, args: DomainHostArgs) -> anyhow::Result<()> {
    let detail = detail_core(state, &args.host).await.map_err(to_anyhow)?;
    let state_label = detail
        .moderation
        .as_ref()
        .map_or("-".to_string(), |m| m.severity.clone());
    println!(
        "host={host} state={state_label} known_actors={known} \
         following={following_accepted} ({following_pending} pending) \
         followers={followers_accepted} ({followers_pending} pending)",
        host = detail.host,
        known = detail.stats.known_actor_count,
        following_accepted = detail.stats.accepted_following_count,
        following_pending = detail.stats.pending_following_count,
        followers_accepted = detail.stats.accepted_followers_count,
        followers_pending = detail.stats.pending_followers_count,
    );
    if let Some(m) = &detail.moderation
        && let Some(reason) = &m.reason
    {
        println!("reason: {reason}");
    }
    println!("-- following --");
    for f in &detail.following {
        println!(
            "  id={id} actor={ap_id} state={state}",
            id = f.follow_id,
            ap_id = f.actor.ap_id,
            state = f.follow_state,
        );
    }
    println!("-- followers --");
    for f in &detail.followers {
        println!(
            "  id={id} actor={ap_id} state={state}",
            id = f.follow_id,
            ap_id = f.actor.ap_id,
            state = f.follow_state,
        );
    }
    Ok(())
}

async fn run_silence(state: &AppState, args: DomainModerateArgs) -> anyhow::Result<()> {
    let row = silence_core(state, &args.host, args.reason)
        .await
        .map_err(to_anyhow)?;
    println!("silenced host={host}", host = row.host);
    Ok(())
}

async fn run_suspend(state: &AppState, args: DomainModerateArgs) -> anyhow::Result<()> {
    let outcome = suspend_core(state, &args.host, args.reason)
        .await
        .map_err(to_anyhow)?;
    println!(
        "suspended host={host} forced_unfollow_count={count}",
        host = outcome.row.host,
        count = outcome.forced_unfollow_count,
    );
    Ok(())
}

async fn run_unset(state: &AppState, args: DomainHostArgs) -> anyhow::Result<()> {
    unset_core(state, &args.host).await.map_err(to_anyhow)?;
    println!("moderation unset for host={host}", host = args.host);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_host_lowercases_and_trims() {
        assert_eq!(
            normalize_host(" Mastodon.EXAMPLE ").unwrap(),
            "mastodon.example"
        );
    }

    #[test]
    fn normalize_host_rejects_empty() {
        assert!(normalize_host("   ").is_err());
    }
}
