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
use crate::net_guard;
use crate::sign::sign_request;
use crate::state::AppState;

pub mod worker;

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
///
/// `inbox_url` は `url::Url::parse` で事前検証する ── DB に投入される文字列が
/// 後段の `reqwest::Url::parse` で必ず通る形であることをここで保証する。
/// 空文字列・`http`/`https` 以外のスキーム・host 欠落はここで弾く。
pub async fn enqueue_activity<'e, E>(
    executor: E,
    sender_actor_id: i64,
    inbox_url: &str,
    activity: &JsonValue,
) -> anyhow::Result<DeliveryQueueRow>
where
    E: sqlx::PgExecutor<'e>,
{
    let url = reqwest::Url::parse(inbox_url)
        .with_context(|| format!("invalid inbox_url {inbox_url:?}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!(
            "inbox_url scheme must be http or https, got {:?}",
            url.scheme()
        );
    }
    if url.host_str().is_none() {
        bail!("inbox_url must have a host component");
    }
    repo::delivery_queue::enqueue(executor, inbox_url, activity, sender_actor_id)
        .await
        .map_err(Into::into)
}

/// 同一 activity を複数 inbox 宛てに **1 INSERT で** enqueue する一括版。
///
/// 各 inbox を [`enqueue_activity`] と同じ基準 (http/https スキーム + host 必須)
/// で検証し、通った分だけを [`repo::delivery_queue::enqueue_batch`] に渡す。
/// 検証で弾いた inbox は `warn` を残してスキップする ── 1 件不正でも残りは
/// 配送する、単発版を for で回したときと同じ挙動。違いは DB 往復が inbox 数に
/// 比例せず常に 1 回で済む点 ([`repo::delivery_queue::enqueue_batch`] 参照)。
///
/// 返り値は実際に enqueue した行数。検証で全部弾かれた / 入力が空なら 0。
pub async fn enqueue_activities<'e, E>(
    executor: E,
    sender_actor_id: i64,
    inbox_urls: &[String],
    activity: &JsonValue,
) -> anyhow::Result<u64>
where
    E: sqlx::PgExecutor<'e>,
{
    let valid: Vec<String> = inbox_urls
        .iter()
        .filter(|inbox| match reqwest::Url::parse(inbox) {
            Ok(url) => {
                let ok =
                    matches!(url.scheme(), "http" | "https") && url.host_str().is_some();
                if !ok {
                    warn!(%inbox, "enqueue_activities: skipping inbox (non-http(s) scheme or no host)");
                }
                ok
            }
            Err(err) => {
                warn!(%inbox, ?err, "enqueue_activities: skipping unparseable inbox_url");
                false
            }
        })
        .cloned()
        .collect();
    repo::delivery_queue::enqueue_batch(executor, &valid, activity, sender_actor_id)
        .await
        .map_err(Into::into)
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
            // Webhook 行は `inbox_url` に Discord の secret token を含む完全 URL が
            // 入っているので、ログ集約先 (Loki / CloudWatch) に流れないよう host
            // までに刈り込む。`list` CLI が URL を host だけ表示する設計と整合させる。
            // (round-1 review F1)
            let inbox_display = redact_inbox_for_log(&row.inbox_url, &row.activity.0);
            info!(
                queue_id,
                status = status.as_u16(),
                inbox = %inbox_display,
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
        Err(err) if err.is_permanent() => {
            // 永続エラー (signing 失敗 / URL 不正 / 内部宛先 / JSON
            // シリアライズ) は retry しても直らない。即座に `dead` に倒し、
            // M3b-3 の常駐 worker ループが `pending` 行を再取得しても同じ
            // 行を永遠にリトライしないようにする (round-2 review F1)。
            // anyhow Err は引き続き返すので CLI 側はエラー詳細を stderr に
            // 出せる。`mark_dead` 自身が失敗した場合 (DB 障害) はその DB
            // エラーを優先して伝えたいので、context を付けて伝播する。
            let reason = err.to_string();
            repo::delivery_queue::mark_dead(state.pool(), queue_id, &reason)
                .await
                .context("mark_dead after permanent failure")?;
            warn!(queue_id, error = %err, "permanent failure; moved to 'dead'");
            Err(anyhow!(
                "outbound delivery refused (moved to 'dead'): {err}"
            ))
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
        // 上の `is_permanent` で Sign / InvalidUrl / BlockedAddress / Serialize
        // は処理済み、`Transport` も直前のアームで処理済み。現時点ではここに
        // 到達するパスは無い。
        //
        // **`AttemptError` に新しい variant を追加するときは必ず明示的なアーム
        // を上に追加すること**。ここに落ちると retry 経路を踏まないため、新規
        // 一時エラー (例: `RateLimit`) を誤って永続失敗扱いしてしまう
        // (round-2 review F4)。
        Err(other) => Err(anyhow!("outbound delivery refused: {other}")),
    }
}

/// `attempt_post` のエラー分類。永続エラー (`Sign` / `InvalidUrl` /
/// `BlockedAddress` / `Serialize` / `MissingPayload` / `WebhookRejected`) と
/// 一時エラー (`Transport`) を呼び出し側で区別するため。
#[derive(Debug, Error)]
enum AttemptError {
    #[error("signing failed: {0}")]
    Sign(#[from] sign_request::SignOutboundError),
    #[error("invalid inbox URL: {0}")]
    InvalidUrl(#[from] url::ParseError),
    #[error("inbox host {host:?} is in a blocked address range ({reason})")]
    BlockedAddress { host: String, reason: &'static str },
    /// `serde_json::Value` のシリアライズ失敗。`Value` 型なら通常起き得ないが、
    /// `expect` で panic させると M3b-3 で常駐 worker ループに移行したあとに
    /// プロセスを落とすリスクが顕在化する。permanent 扱いで `dead` に倒す。
    #[error("activity JSON serialization failed: {0}")]
    Serialize(#[from] serde_json::Error),
    /// Webhook 行で `activity.payload` キーが欠落している。dispatch 側の契約違反
    /// または DB 直挿入の人為ミスなので retry しても直らない。
    /// (round-3 review F3: `expect_err` で `serde_json::Error` を偽造する代わりに
    /// 専用 variant を持って panic リスクを除去する)
    #[error("activity JSON missing 'payload' key")]
    MissingPayload,
    /// Webhook 配送で Discord / Slack から永続失敗扱いのステータス (401 / 403 /
    /// 404 / 410) が返った。トークン無効 / channel 削除 / 権限剥奪は retry しても
    /// 回復しないので即 `dead` に倒す。AP 配送の 4xx は Mastodon の retry 慣習に
    /// 合わせて一時失敗扱いだが、webhook ではここで分岐する。
    /// (round-3 review F2)
    #[error("webhook rejected with HTTP {status} (permanent)")]
    WebhookRejected { status: u16 },
    #[error("transport: {0}")]
    Transport(#[from] reqwest::Error),
}

impl AttemptError {
    /// 恒久エラーかどうか。`true` の場合はリトライしても無駄なので、
    /// queue 行は触らずに上位 (CLI) に伝播させる。
    fn is_permanent(&self) -> bool {
        matches!(
            self,
            Self::Sign(_)
                | Self::InvalidUrl(_)
                | Self::BlockedAddress { .. }
                | Self::Serialize(_)
                | Self::MissingPayload
                | Self::WebhookRejected { .. }
        )
    }
}

/// 1 回 POST を撃つだけのヘルパ。
async fn attempt_post(
    state: &AppState,
    row: &DeliveryQueueRow,
    sender: &ActorRow,
) -> Result<StatusCode, AttemptError> {
    // **SSRF 最小ガード**: `inbox_url` を解析し、host が private / loopback /
    // link-local / reserved の IP literal なら配送を拒否する。これは
    // 「攻撃者が DB 書き込みで `http://192.168.0.1/admin` のような行を
    // 仕込んでも内部サービスを叩けない」ことを担保する最小防御。
    //
    // 完全な SSRF 対策 (DNS 解決後の再検証 / CIDR allowlist / 自前 connector
    // による socket-level チェック) は CLAUDE.md §3 が「外部 URL 取得は
    // 必ず media-proxy 経由」と定めている通り、本体ではなく media-proxy 側で
    // 行う。M3b-3 で remote actor fetch を実装する際に media-proxy 配送経路へ
    // 統合する想定。
    //
    // [`AppState::allow_internal_inbox`] が `true` の場合 (テスト経路のみ)
    // はガードを緩める ── 統合テスト用 inbox を loopback で立てるため。
    // 本番 `from_config` 経由では常に `false` で、ガードはバイパスされない。
    let url = reqwest::Url::parse(&row.inbox_url)?;

    // **自己 inbox 宛の配送ループ防止**: `inbox_url` の host が自インスタンスの
    // 公開ホスト名と一致する場合は即座に拒否する。M3b-3 で常駐 worker ループに
    // したあと、バグや悪意ある DB 書き込みで自分宛行が混入すると無限ループする。
    // 防御深度として `allow_internal_inbox` のテストフラグに関わらず常に適用する
    // ── テストでは inbox host を `127.0.0.1` などで立てるので、本番 host
    // (`example.test` 等) と衝突しない設計になっている。
    if net_guard::is_self_host(&url, &state.config().server.host) {
        return Err(AttemptError::BlockedAddress {
            host: url.host_str().unwrap_or("").to_string(),
            reason: "self-delivery loop",
        });
    }

    if !state.allow_internal_inbox()
        && let Some(reason) = net_guard::host_blocked(&url)
    {
        return Err(AttemptError::BlockedAddress {
            host: url.host_str().unwrap_or("").to_string(),
            reason,
        });
    }

    // **Webhook 通知の分岐**: `activity.type` が `"Webhook:"` prefix なら、
    // ActivityPub の HTTP 署名は付けず、`payload` サブツリーを
    // `application/json` で POST する ── Discord / Slack / Misskey 互換 webhook
    // への通知配送 (`crate::notification::dispatch`)。
    //
    // net_guard / self-host 検査は上で通常経路と同じく適用済み。`sender` の鍵は
    // 触らないが、`sender_actor_id` 列は NOT NULL 制約で埋まっている前提
    // (notification dispatch 側で local actor の id を入れている)。
    let activity_type = row
        .activity
        .0
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if activity_type.starts_with("Webhook:") {
        return attempt_post_webhook(state, &row.activity.0, url).await;
    }

    // `serde_json::Value` のシリアライズは実質失敗しないが、`expect` だと
    // 常駐 worker ループ化後にプロセス落ちのリスクが残る。`Serialize` variant
    // で permanent 扱い (`dead` に倒す) に伝播する (#22)。
    let body = serde_json::to_vec(&row.activity.0)?;

    let mut req = state
        .http_client()
        .post(url)
        // Mastodon / Misskey / Pleroma 全部受理する Content-Type。
        // `application/ld+json; profile=...` でもよいが、最大互換は本値。
        .header("content-type", "application/activity+json")
        .body(body)
        .build()?;

    sign_request::sign_outbox_request(&mut req, sender)?;

    let response = state.http_client().execute(req).await?;
    Ok(response.status())
}

/// Webhook 通知 (`activity.type` が `Webhook:Discord` / `Webhook:Plain`) の
/// 配送本体。`activity.payload` だけを `application/json` で POST する。署名
/// は一切付けない。
///
/// `activity.payload` が無い行は **permanent error** (`MissingPayload` で
/// `dead` に倒す) ── notification dispatch 側が必ず `payload` を埋める契約
/// なので、欠如は DB 直挿入の人為ミス。retry しても直らない。
///
/// 受信ステータスのうち **401 / 403 / 404 / 410** は Discord / Slack いずれの
/// webhook でも「トークン失効・channel 削除・権限剥奪」を意味し retry で
/// 回復しない。AP 配送と違って Mastodon 流の retry 慣習が無いので
/// [`AttemptError::WebhookRejected`] で即 dead に倒す。それ以外の非 2xx
/// (5xx / 408 / 429 等) は `Ok(status)` のまま返して上位の指数バックオフに
/// 任せる。
async fn attempt_post_webhook(
    state: &AppState,
    activity: &JsonValue,
    url: reqwest::Url,
) -> Result<StatusCode, AttemptError> {
    let payload = activity
        .get("payload")
        .ok_or(AttemptError::MissingPayload)?;
    let body = serde_json::to_vec(payload)?;
    let req = state
        .http_client()
        .post(url)
        .header("content-type", "application/json")
        .body(body)
        .build()?;
    let response = state.http_client().execute(req).await?;
    let status = response.status();
    if is_webhook_permanent_status(status) {
        return Err(AttemptError::WebhookRejected {
            status: status.as_u16(),
        });
    }
    Ok(status)
}

/// 4xx のうち webhook では永続失敗とみなすもの。`401` / `403` / `404` / `410`。
/// Discord / Slack ともに「トークン無効 / channel 削除 / 権限なし」がここに
/// マップされる。それ以外の 4xx (`400` 等) も理屈上は retry 無意味だが、payload
/// 構築のバグ起因 = 修正 deploy で直る可能性があるので safe side で retry させる。
fn is_webhook_permanent_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND | StatusCode::GONE
    )
}

/// `info!` / `warn!` 用に inbox URL をマスクする。
///
/// Webhook 行 (`activity.type` が `Webhook:` prefix) は `inbox_url` が Discord
/// webhook の full URL = secret token を含むので、`<scheme>://<host>/<redacted>`
/// に刈り込む。AP 配送行は素の URL をそのまま返す (連合配送 inbox は public
/// path なので秘匿不要、デバッグ可読性を優先)。
/// (round-1 review F1)
fn redact_inbox_for_log(raw: &str, activity: &JsonValue) -> String {
    let activity_type = activity
        .get("type")
        .and_then(JsonValue::as_str)
        .unwrap_or("");
    if !activity_type.starts_with("Webhook:") {
        return raw.to_string();
    }
    reqwest::Url::parse(raw)
        .ok()
        .and_then(|u| {
            u.host_str()
                .map(|h| format!("{scheme}://{h}/<redacted>", scheme = u.scheme()))
        })
        .unwrap_or_else(|| "<invalid-webhook-url>".to_string())
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
    fn attempt_error_permanent_classification() {
        let ssrf = AttemptError::BlockedAddress {
            host: "127.0.0.1".into(),
            reason: "loopback",
        };
        assert!(ssrf.is_permanent());
        let bad = AttemptError::InvalidUrl(url::ParseError::EmptyHost);
        assert!(bad.is_permanent());
        // `Serialize` も permanent — `Value` 由来の to_vec はまず失敗しないが、
        // 万一起きたら retry で直る性質のエラーではない (#22)。
        let serde_err = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
        let ser = AttemptError::Serialize(serde_err);
        assert!(ser.is_permanent());
        // round-3 review F2/F3: webhook 永続失敗系も permanent。
        assert!(AttemptError::MissingPayload.is_permanent());
        assert!(AttemptError::WebhookRejected { status: 401 }.is_permanent());
        assert!(AttemptError::WebhookRejected { status: 404 }.is_permanent());
    }

    /// `is_webhook_permanent_status` の境界。401/403/404/410 が permanent。
    /// 5xx / 429 / 408 等は retry させたいので false。
    #[test]
    fn webhook_permanent_status_boundaries() {
        assert!(is_webhook_permanent_status(StatusCode::UNAUTHORIZED));
        assert!(is_webhook_permanent_status(StatusCode::FORBIDDEN));
        assert!(is_webhook_permanent_status(StatusCode::NOT_FOUND));
        assert!(is_webhook_permanent_status(StatusCode::GONE));
        // retry させたい群
        assert!(!is_webhook_permanent_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(!is_webhook_permanent_status(StatusCode::REQUEST_TIMEOUT));
        assert!(!is_webhook_permanent_status(StatusCode::BAD_GATEWAY));
        assert!(!is_webhook_permanent_status(
            StatusCode::SERVICE_UNAVAILABLE
        ));
        assert!(!is_webhook_permanent_status(StatusCode::GATEWAY_TIMEOUT));
        // 400 はバグ起因の可能性があるので一旦 retry 扱い
        assert!(!is_webhook_permanent_status(StatusCode::BAD_REQUEST));
        // 2xx は当然 permanent ではない
        assert!(!is_webhook_permanent_status(StatusCode::OK));
    }

    /// round-1 review F1: Webhook 行は secret token を含む URL を host 止まり
    /// にマスクする。AP 配送行はそのまま (連合 inbox は public path)。
    #[test]
    fn redact_inbox_for_log_masks_webhook_only() {
        let webhook_activity = serde_json::json!({
            "type": "Webhook:Discord",
            "channel_id": 1,
            "event": "mention",
            "payload": {},
        });
        let redacted = redact_inbox_for_log(
            "https://discord.com/api/webhooks/123456789/SUPER_SECRET_TOKEN",
            &webhook_activity,
        );
        assert_eq!(redacted, "https://discord.com/<redacted>");
        // path / query / token は完全に消える
        assert!(!redacted.contains("SUPER_SECRET_TOKEN"));
        assert!(!redacted.contains("webhooks"));

        // AP 配送行は素のまま
        let ap_activity = serde_json::json!({ "type": "Create" });
        let raw = "https://mastodon.example/users/alice/inbox";
        assert_eq!(redact_inbox_for_log(raw, &ap_activity), raw);

        // 不正 URL でも panic しない
        let bad = redact_inbox_for_log("not a url", &webhook_activity);
        assert_eq!(bad, "<invalid-webhook-url>");
    }

    /// `Webhook:Plain` prefix も同様にマスクされる。
    #[test]
    fn redact_inbox_for_log_handles_webhook_plain() {
        let webhook_activity = serde_json::json!({
            "type": "Webhook:Plain",
            "payload": {},
        });
        let redacted = redact_inbox_for_log(
            "https://example.slack.com/services/T000/B000/SECRET",
            &webhook_activity,
        );
        assert_eq!(redacted, "https://example.slack.com/<redacted>");
    }

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
