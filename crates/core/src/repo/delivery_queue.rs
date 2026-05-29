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

pub async fn enqueue(
    pool: &PgPool,
    inbox_url: &str,
    activity: &JsonValue,
    sender_actor_id: i64,
) -> sqlx::Result<DeliveryQueueRow> {
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
    .fetch_one(pool)
    .await
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
