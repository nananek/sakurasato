//! Compile-time checked queries against the `delivery_queue` table.
//!
//! The lease/picker semantics (selecting due rows and updating their state
//! transactionally) are deferred to M3 when the delivery worker is built.

use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;
use sqlx::PgPool;
use sqlx::types::Json;

use crate::model::DeliveryQueueRow;

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

/// Mark a delivery as failed and schedule the next attempt. Used by the M3
/// delivery worker after a non-fatal HTTP error.
pub async fn mark_failed(
    pool: &PgPool,
    id: i64,
    last_error: &str,
    next_attempt_at: DateTime<Utc>,
) -> sqlx::Result<()> {
    sqlx::query!(
        r#"
        UPDATE delivery_queue
        SET attempts = attempts + 1,
            last_error = $1,
            next_attempt_at = $2,
            state = 'failed',
            updated_at = now()
        WHERE id = $3
        "#,
        last_error,
        next_attempt_at,
        id,
    )
    .execute(pool)
    .await
    .map(|_| ())
}
