//! Compile-time checked queries against the `actor` table.

use sqlx::PgPool;
use sqlx::types::Json;

use crate::model::ActorRow;

/// Fields required to insert a new actor row. Database-managed columns
/// (`id`, `created_at`, `updated_at`) are not part of this struct.
#[derive(Debug, Clone)]
pub struct NewActor {
    pub ap_id: String,
    pub preferred_username: String,
    pub host: String,
    pub display_name: Option<String>,
    pub summary: Option<String>,
    pub icon_url: Option<String>,
    pub image_url: Option<String>,
    pub inbox_url: String,
    pub shared_inbox_url: Option<String>,
    pub outbox_url: Option<String>,
    pub followers_url: Option<String>,
    pub following_url: Option<String>,
    pub public_key_id: String,
    pub public_key_pem: String,
    pub private_key_pem: Option<String>,
    pub also_known_as: Vec<String>,
    pub moved_to_ap_id: Option<String>,
    pub is_local: bool,
    pub actor_type: String,
}

/// Insert a new actor and return the persisted row.
pub async fn insert(pool: &PgPool, new: NewActor) -> sqlx::Result<ActorRow> {
    let also_known_as_json = serde_json::to_value(&new.also_known_as)
        .expect("Vec<String> is always serializable to JSON");
    sqlx::query_as!(
        ActorRow,
        r#"
        INSERT INTO actor (
            ap_id, preferred_username, host, display_name, summary,
            icon_url, image_url, inbox_url, shared_inbox_url, outbox_url,
            followers_url, following_url, public_key_id, public_key_pem,
            private_key_pem, also_known_as, moved_to_ap_id, is_local, actor_type
        )
        VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
            $11, $12, $13, $14, $15, $16, $17, $18, $19
        )
        RETURNING
            id, ap_id, preferred_username, host, display_name, summary,
            icon_url, image_url, inbox_url, shared_inbox_url, outbox_url,
            followers_url, following_url, public_key_id, public_key_pem,
            private_key_pem,
            also_known_as as "also_known_as: Json<Vec<String>>",
            moved_to_ap_id, is_local, actor_type, fetched_at, created_at, updated_at
        "#,
        new.ap_id,
        new.preferred_username,
        new.host,
        new.display_name,
        new.summary,
        new.icon_url,
        new.image_url,
        new.inbox_url,
        new.shared_inbox_url,
        new.outbox_url,
        new.followers_url,
        new.following_url,
        new.public_key_id,
        new.public_key_pem,
        new.private_key_pem,
        also_known_as_json,
        new.moved_to_ap_id,
        new.is_local,
        new.actor_type,
    )
    .fetch_one(pool)
    .await
}

/// Look an actor up by its `id` primary key.
pub async fn get_by_id(pool: &PgPool, id: i64) -> sqlx::Result<Option<ActorRow>> {
    sqlx::query_as!(
        ActorRow,
        r#"
        SELECT
            id, ap_id, preferred_username, host, display_name, summary,
            icon_url, image_url, inbox_url, shared_inbox_url, outbox_url,
            followers_url, following_url, public_key_id, public_key_pem,
            private_key_pem,
            also_known_as as "also_known_as: Json<Vec<String>>",
            moved_to_ap_id, is_local, actor_type, fetched_at, created_at, updated_at
        FROM actor WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await
}

/// Look an actor up by its `ap_id` (canonical `ActivityPub` URI).
pub async fn get_by_ap_id(pool: &PgPool, ap_id: &str) -> sqlx::Result<Option<ActorRow>> {
    sqlx::query_as!(
        ActorRow,
        r#"
        SELECT
            id, ap_id, preferred_username, host, display_name, summary,
            icon_url, image_url, inbox_url, shared_inbox_url, outbox_url,
            followers_url, following_url, public_key_id, public_key_pem,
            private_key_pem,
            also_known_as as "also_known_as: Json<Vec<String>>",
            moved_to_ap_id, is_local, actor_type, fetched_at, created_at, updated_at
        FROM actor WHERE ap_id = $1
        "#,
        ap_id,
    )
    .fetch_optional(pool)
    .await
}

/// Look an actor up by the (`preferred_username`, `host`) pair.
/// Useful for `WebFinger` lookups.
pub async fn get_by_username_host(
    pool: &PgPool,
    preferred_username: &str,
    host: &str,
) -> sqlx::Result<Option<ActorRow>> {
    sqlx::query_as!(
        ActorRow,
        r#"
        SELECT
            id, ap_id, preferred_username, host, display_name, summary,
            icon_url, image_url, inbox_url, shared_inbox_url, outbox_url,
            followers_url, following_url, public_key_id, public_key_pem,
            private_key_pem,
            also_known_as as "also_known_as: Json<Vec<String>>",
            moved_to_ap_id, is_local, actor_type, fetched_at, created_at, updated_at
        FROM actor WHERE preferred_username = $1 AND host = $2
        "#,
        preferred_username,
        host,
    )
    .fetch_optional(pool)
    .await
}

/// Touch the `updated_at` column and refresh `fetched_at` for a remote actor.
/// Used when re-fetching actor metadata from a remote server.
pub async fn mark_fetched(pool: &PgPool, id: i64) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE actor SET fetched_at = now(), updated_at = now() WHERE id = $1",
        id,
    )
    .execute(pool)
    .await
    .map(|_| ())
}
