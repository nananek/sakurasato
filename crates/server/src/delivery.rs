//! アウトバウンド配送ワーカ (M3b-2 PR2: スケルトン)。
//!
//! 役割:
//! 1. `enqueue_activity` — 任意の Activity JSON を `delivery_queue` に push。
//!    M3b-3 で `Follow` / `Accept` / `Create` の dispatch から呼ばれる想定。
//! 2. `try_deliver_one` — 単一 queue 行をピックして相手 inbox に POST。
//!    cavage RSA-SHA256 で署名し、結果に応じて `delivered` / `failed` /
//!    `dead` に倒す。
//!
//! **本 PR では retry/backoff の本格化と常駐ループは作らない**。M3b-3 で
//! `tokio::spawn` した worker ループにする。PR2 は CLI 経由 (`sakurasato
//! deliver --queue-id N`) で 1 行だけ flush できれば足りる ── ローカル
//! federation テスト (M3b-2 PR3) で「投げてみる」を回すための最小機構。
//!
//! ## バックオフ
//!
//! M3b-3 で正式化するが、PR2 でも `mark_failed` を呼ぶ以上は何らかの値を
//! 入れる必要がある。`2^attempts` 分 (上限 1 時間) の指数で当面しのぐ。

use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use chrono::{DateTime, Utc};
use reqwest::StatusCode;
use sakurasato_core::model::{ActorRow, DeliveryQueueRow, DeliveryState};
use sakurasato_core::repo;
use serde_json::Value as JsonValue;
use sqlx::PgPool;
use thiserror::Error;
use tracing::{info, warn};

use crate::cli::DeliverArgs;
use crate::sign::sign_request;
use crate::state::AppState;

/// 1 回の配送試行の結果。CLI / 将来のワーカループが状態を表示するために使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryOutcome {
    /// 2xx を受領、`delivered` に倒した。
    Delivered,
    /// 一時失敗。`failed` に倒し、`next_attempt_at` 以降に再試行可能。
    Retry,
    /// `attempts` が上限に達して `dead` に倒した。
    Dead,
}

/// 上限到達までの最大試行回数。
const MAX_ATTEMPTS: i32 = repo::delivery_queue::DEFAULT_MAX_ATTEMPTS;

/// バックオフの上限 (1 時間)。M3b-3 の常駐ワーカ化で見直す。
const BACKOFF_CAP: Duration = Duration::from_hours(1);

/// CLI `sakurasato deliver --queue-id N` の実体。
///
/// 1 行ぶん試行し、結果を stdout に表示する。`Serve` と違って migrations は
/// 走らせない (`init` で済んでいる前提)。
pub async fn run(config: sakurasato_core::Config, args: DeliverArgs) -> anyhow::Result<()> {
    let state = AppState::from_config(config).await?;
    let outcome = try_deliver_one(&state, args.queue_id).await?;
    match outcome {
        DeliveryOutcome::Delivered => {
            info!(queue_id = args.queue_id, "delivered");
            println!("queue {} delivered", args.queue_id);
        }
        DeliveryOutcome::Retry => {
            info!(queue_id = args.queue_id, "scheduled for retry");
            println!("queue {} failed; scheduled for retry", args.queue_id);
        }
        DeliveryOutcome::Dead => {
            warn!(queue_id = args.queue_id, "queue exhausted retries");
            println!(
                "queue {} reached max attempts; moved to 'dead'",
                args.queue_id
            );
        }
    }
    Ok(())
}

/// 任意の Activity JSON を配送キューに追加する。
///
/// 重複排除や宛先展開 (`shared_inbox` 集約等) は M3b-3 の責務。PR2 では
/// 「指定 inbox URL に 1 行 push する」 だけの最小 API。
pub async fn enqueue_activity(
    pool: &PgPool,
    sender_actor_id: i64,
    inbox_url: &str,
    activity: &JsonValue,
) -> sqlx::Result<DeliveryQueueRow> {
    repo::delivery_queue::enqueue(pool, inbox_url, activity, sender_actor_id).await
}

/// 指定 queue id を 1 回試行する。
///
/// 終端状態 (`delivered` / `dead`) に到達済みの行は **何もせず**
/// `anyhow::Error` を返す ── 誤って状態を巻き戻さないため。
pub async fn try_deliver_one(state: &AppState, queue_id: i64) -> anyhow::Result<DeliveryOutcome> {
    let row = repo::delivery_queue::get_by_id(state.pool(), queue_id)
        .await
        .with_context(|| format!("fetch delivery_queue id {queue_id}"))?
        .ok_or_else(|| anyhow!("delivery_queue id {queue_id} not found"))?;

    // 終端状態の行は触らない (mark_* 系も WHERE で弾くが、ここで早めに
    // 失敗を返したほうが CLI ユーザに明示できる)。
    if row.state == DeliveryState::Delivered.as_str() || row.state == DeliveryState::Dead.as_str() {
        bail!(
            "delivery_queue id {queue_id} is already in terminal state '{}'",
            row.state
        );
    }

    let sender = repo::actor::get_by_id(state.pool(), row.sender_actor_id)
        .await
        .with_context(|| format!("fetch sender actor id {}", row.sender_actor_id))?
        .ok_or_else(|| anyhow!("sender actor {} not found", row.sender_actor_id))?;

    // local actor だけが秘密鍵を持つ。remote actor からの配送は意味を成さない。
    if !sender.is_local {
        bail!(
            "sender actor {} is not local; cannot sign outbound delivery",
            sender.ap_id
        );
    }

    match attempt_post(state, &row, &sender).await {
        Ok(status) if status.is_success() => {
            info!(
                queue_id,
                status = status.as_u16(),
                inbox = %row.inbox_url,
                "delivery succeeded",
            );
            repo::delivery_queue::mark_delivered(state.pool(), queue_id)
                .await
                .context("mark_delivered")?;
            Ok(DeliveryOutcome::Delivered)
        }
        Ok(status) => {
            // 受け手から非 2xx。Mastodon 系は 401 を「actor 未知 → 再送」
            // と読む慣習があり、4xx でも全部 dead に倒すと PR1 で意図した
            // 再送ループ ([[m3b-followup-plan]]) と整合しなくなる。PR2 では
            // 一律 retry 扱いで `mark_failed` に倒し、attempts 上限で dead
            // に転がす設計。区分けは M3b-3 で詳細化。
            schedule_retry(
                state.pool(),
                queue_id,
                row.attempts,
                &format!("HTTP {}", status.as_u16()),
            )
            .await
        }
        Err(AttemptError::Sign(e)) => {
            // 署名できない (秘密鍵が無い等) のは状態 / 設定の不整合で、
            // retry しても直らない恒久障害。queue 行は触らずに上位へ返し、
            // CLI ユーザに即座に気付かせる ── 自動で `dead` に倒すよりも
            // 原因を表示するほうが M3b-2 段階では役立つ。M3b-3 で
            // worker ループ化したときに「permanent failure」分類で
            // `dead` に倒す予定。
            Err(anyhow!("outbound signing failed: {e}"))
        }
        Err(AttemptError::Transport(transport_err)) => {
            warn!(queue_id, error = %transport_err, "delivery transport error");
            schedule_retry(
                state.pool(),
                queue_id,
                row.attempts,
                &format!("transport error: {transport_err}"),
            )
            .await
        }
    }
}

/// `attempt_post` のエラー二分。署名失敗 (恒久) と HTTP transport 失敗
/// (一時) を呼び出し側で区別するため。
#[derive(Debug, Error)]
enum AttemptError {
    #[error("signing failed: {0}")]
    Sign(#[from] sign_request::SignOutboundError),
    #[error("transport: {0}")]
    Transport(#[from] reqwest::Error),
}

/// 1 回 POST を撃つだけのヘルパ。
async fn attempt_post(
    state: &AppState,
    row: &DeliveryQueueRow,
    sender: &ActorRow,
) -> Result<StatusCode, AttemptError> {
    let body = serde_json::to_vec(&row.activity.0).expect("Value is always JSON-serializable");

    let mut req = state
        .http_client()
        .post(&row.inbox_url)
        // Mastodon / Misskey / Pleroma 全部受理する Content-Type。
        // `application/ld+json; profile=...` でもよいが、最大互換は本値。
        .header("content-type", "application/activity+json")
        .body(body)
        .build()?;

    sign_request::sign_outbox_request(&mut req, sender)?;

    let response = state.http_client().execute(req).await?;
    Ok(response.status())
}

/// `mark_failed` を呼び、`Retry` か `Dead` を返す。
async fn schedule_retry(
    pool: &PgPool,
    queue_id: i64,
    current_attempts: i32,
    last_error: &str,
) -> anyhow::Result<DeliveryOutcome> {
    let next_attempts = current_attempts.saturating_add(1);
    let next_at = compute_backoff(Utc::now(), next_attempts);
    repo::delivery_queue::mark_failed(pool, queue_id, last_error, next_at, MAX_ATTEMPTS)
        .await
        .context("mark_failed")?;
    if next_attempts >= MAX_ATTEMPTS {
        warn!(queue_id, "delivery reached max attempts; moved to 'dead'");
        Ok(DeliveryOutcome::Dead)
    } else {
        info!(
            queue_id,
            next_attempts, next_at = %next_at, "delivery scheduled for retry",
        );
        Ok(DeliveryOutcome::Retry)
    }
}

/// `2^attempts` 分の指数バックオフを返す (上限 [`BACKOFF_CAP`])。
///
/// `attempts` は **この試行を含む** 値 (= `current + 1`)。例えば 1 回目失敗
/// 直後は `attempts=1` で次回まで 2 分、3 回目失敗で 8 分、…と伸びる。
/// shift overflow を避けるため 12 でクランプ (2^12 = 4096 秒 ≈ 68 分)。
fn compute_backoff(now: DateTime<Utc>, attempts: i32) -> DateTime<Utc> {
    let clamped = attempts.clamp(1, 12);
    // `1 << clamped` で 2^attempts を作る。`clamped <= 12` なので i32 範囲内。
    let secs = u64::from(1u32 << clamped) * 60;
    let raw = Duration::from_secs(secs).min(BACKOFF_CAP);
    now + chrono::Duration::from_std(raw).expect("BACKOFF_CAP fits in chrono::Duration")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_exponentially_until_cap() {
        let now = Utc::now();
        // attempts=1 → 2 分、 attempts=3 → 8 分、 attempts=12 → cap (1h)。
        let a1 = compute_backoff(now, 1);
        let a3 = compute_backoff(now, 3);
        let a12 = compute_backoff(now, 12);
        assert!(a1 > now);
        assert!(a3 > a1);
        // cap に張り付くので a12 は 1 時間後ぴったり。
        let diff = (a12 - now).num_seconds();
        assert_eq!(diff, i64::try_from(BACKOFF_CAP.as_secs()).unwrap());
    }

    #[test]
    fn backoff_clamps_negative_and_zero() {
        let now = Utc::now();
        // attempts=0 / 負値も最低 2 分は確保する (clamp の下端 1)。
        let a0 = compute_backoff(now, 0);
        let neg = compute_backoff(now, -5);
        assert!(a0 > now);
        assert_eq!(a0, neg);
    }
}
