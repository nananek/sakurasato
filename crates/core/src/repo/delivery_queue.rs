//! Compile-time checked queries against the `delivery_queue` table.
//!
//! The lease/picker semantics (selecting due rows and updating their state
//! transactionally) are deferred to M3 when the delivery worker is built.

use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;
use sqlx::PgPool;
use sqlx::types::Json;

use crate::model::DeliveryQueueRow;

/// Default maximum delivery attempts before a queue row is permanently
/// retired into the `dead` state. The worker can override this per call.
pub const DEFAULT_MAX_ATTEMPTS: i32 = 10;

/// `delivery_queue` に 1 行 push する。
///
/// `executor` を generic にしてあるので `&PgPool` (= 単発) と
/// `&mut Transaction` (= 周囲の DB 変更とまとめてコミット) のどちらでも
/// 呼べる。PR #80 round-2 で `follow-request approve/reject` が
/// `set_state` と enqueue を同一トランザクションで囲むために generic 化。
pub async fn enqueue<'e, E>(
    executor: E,
    inbox_url: &str,
    activity: &JsonValue,
    sender_actor_id: i64,
) -> sqlx::Result<DeliveryQueueRow>
where
    E: sqlx::PgExecutor<'e>,
{
    sqlx::query_as!(
        DeliveryQueueRow,
        r#"
        INSERT INTO delivery_queue (inbox_url, activity, sender_actor_id)
        VALUES ($1, $2, $3)
        RETURNING
            id, inbox_url,
            activity as "activity: Json<JsonValue>",
            sender_actor_id, attempts, next_attempt_at, last_error, state,
            created_at, updated_at
        "#,
        inbox_url,
        activity,
        sender_actor_id,
    )
    .fetch_one(executor)
    .await
}

/// 同一 activity を複数 inbox 宛てに **1 文で** 一括 enqueue する。
///
/// [`enqueue`] を inbox ごとに呼ぶと N 回の INSERT を直列に await することになり、
/// お一人様 server でもリモートフォロワーが増えると 1 投稿あたりの
/// `delivery_queue` 書き込みが N 往復に膨らむ。managed Postgres (Neon 等、
/// 1 query = 1 ネットワーク往復) では投稿レスポンスの体感遅延に直結する。
/// `unnest` で N 行を 1 INSERT に畳んで往復を 1 に抑える。
///
/// `inbox_urls` は呼び出し側で重複除去・URL 検証済みであること (本関数は
/// 素通しで INSERT する)。返り値は実際に挿入された行数。空配列なら 0。
pub async fn enqueue_batch<'e, E>(
    executor: E,
    inbox_urls: &[String],
    activity: &JsonValue,
    sender_actor_id: i64,
) -> sqlx::Result<u64>
where
    E: sqlx::PgExecutor<'e>,
{
    if inbox_urls.is_empty() {
        return Ok(0);
    }
    let result = sqlx::query!(
        r#"
        INSERT INTO delivery_queue (inbox_url, activity, sender_actor_id)
        SELECT u, $2, $3
        FROM unnest($1::text[]) AS u
        "#,
        inbox_urls,
        activity,
        sender_actor_id,
    )
    .execute(executor)
    .await?;
    Ok(result.rows_affected())
}

/// Pick up to `limit` due rows from the queue.
///
/// "Due" means `state IN ('pending', 'failed')` and `next_attempt_at <= now()`.
/// Returns rows ordered by `next_attempt_at` ascending so the oldest backlog
/// is drained first.
///
/// **No locking / atomic claim** — the M3b-3 single-process worker loop runs
/// one instance, so concurrent claim races are not a concern. M4+ (when a
/// SSE-based worker is split out) will need `FOR UPDATE SKIP LOCKED`.
pub async fn pick_due(pool: &PgPool, limit: i64) -> sqlx::Result<Vec<DeliveryQueueRow>> {
    sqlx::query_as!(
        DeliveryQueueRow,
        r#"
        SELECT
            id, inbox_url,
            activity as "activity: Json<JsonValue>",
            sender_actor_id, attempts, next_attempt_at, last_error, state,
            created_at, updated_at
        FROM delivery_queue
        WHERE state IN ('pending', 'failed')
          AND next_attempt_at <= now()
        ORDER BY next_attempt_at ASC
        LIMIT $1
        "#,
        limit,
    )
    .fetch_all(pool)
    .await
}

/// まだ配送しきっていない行 (`state IN ('pending','failed')`) のうち、**最も早い**
/// `next_attempt_at` を返す。1 件も無ければ `None`。
///
/// 配送ワーカが「空ポーリング」をやめてアイドルに眠る際、次にリトライが
/// due になる時刻を 1 回だけ引くために使う。`None` のときは未配送行ゼロ
/// なので、ワーカは通知 (`AppState::wake_delivery`) が来るまで DB を一切
/// 叩かずに眠れる ── これが serverless Postgres の autosuspend を可能にする。
pub async fn next_due_at(pool: &PgPool) -> sqlx::Result<Option<DateTime<Utc>>> {
    let row = sqlx::query_scalar!(
        r#"
        SELECT min(next_attempt_at) AS "next: DateTime<Utc>"
        FROM delivery_queue
        WHERE state IN ('pending', 'failed')
        "#,
    )
    .fetch_one(pool)
    .await?;
    Ok(row)
}

pub async fn get_by_id(pool: &PgPool, id: i64) -> sqlx::Result<Option<DeliveryQueueRow>> {
    sqlx::query_as!(
        DeliveryQueueRow,
        r#"
        SELECT
            id, inbox_url,
            activity as "activity: Json<JsonValue>",
            sender_actor_id, attempts, next_attempt_at, last_error, state,
            created_at, updated_at
        FROM delivery_queue WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await
}

/// Mark a delivery row as successfully delivered.
///
/// Only `pending` / `failed` rows are touched, so a delayed worker call can't
/// move a row out of a terminal state (`delivered` / `dead`). This is the
/// success-side counterpart to [`mark_failed`].
pub async fn mark_delivered(pool: &PgPool, id: i64) -> sqlx::Result<()> {
    sqlx::query!(
        r#"
        UPDATE delivery_queue
        SET state = 'delivered',
            attempts = attempts + 1,
            last_error = NULL,
            updated_at = now()
        WHERE id = $1 AND state IN ('pending', 'failed')
        "#,
        id,
    )
    .execute(pool)
    .await
    .map(|_| ())
}

/// Mark a delivery as permanently dead with the given failure reason.
///
/// Use this when the failure is known to be non-recoverable (signing failure,
/// SSRF guard hit, JSON serialization failure, etc.) so the queue row should
/// never be retried. `attempts` is not incremented — the row goes straight
/// from `pending` / `failed` to `dead` regardless of `max_attempts`.
/// Terminal rows (`delivered` / `dead`) are not touched, so a delayed worker
/// call cannot revert state.
pub async fn mark_dead(pool: &PgPool, id: i64, last_error: &str) -> sqlx::Result<()> {
    sqlx::query!(
        r#"
        UPDATE delivery_queue
        SET state = 'dead',
            last_error = $1,
            updated_at = now()
        WHERE id = $2 AND state IN ('pending', 'failed')
        "#,
        last_error,
        id,
    )
    .execute(pool)
    .await
    .map(|_| ())
}

/// Mark a delivery as failed and schedule the next attempt.
///
/// Behaviour:
/// - Increments `attempts`.
/// - If the new `attempts` reaches `max_attempts`, the row is moved to
///   `state = 'dead'` so the worker will never pick it again.
/// - Otherwise the row goes back to `state = 'failed'` and the worker
///   will re-lease it after `next_attempt_at`.
/// - **Only `pending` / `failed` rows are affected.** `delivered` rows stay
///   delivered, and `dead` rows stay dead, so a stale or misrouted worker
///   call cannot revert a terminal state.
///
/// `max_attempts` is parameterised so the worker can tune it (e.g. raise
/// it temporarily during a known remote outage). Use
/// [`DEFAULT_MAX_ATTEMPTS`] otherwise.
pub async fn mark_failed(
    pool: &PgPool,
    id: i64,
    last_error: &str,
    next_attempt_at: DateTime<Utc>,
    max_attempts: i32,
) -> sqlx::Result<()> {
    sqlx::query!(
        r#"
        UPDATE delivery_queue
        SET attempts = attempts + 1,
            last_error = $1,
            next_attempt_at = $2,
            state = CASE
                WHEN attempts + 1 >= $3 THEN 'dead'
                ELSE 'failed'
            END,
            updated_at = now()
        WHERE id = $4 AND state IN ('pending', 'failed')
        "#,
        last_error,
        next_attempt_at,
        max_attempts,
        id,
    )
    .execute(pool)
    .await
    .map(|_| ())
}
