//! Compile-time checked queries against the `api_token` table.
//!
//! Used by the M4 local-API auth middleware. The repo never accepts the raw
//! Bearer token — callers must hash it (SHA-256 hex) in the server crate
//! (`token::hash`) and pass the hash. This keeps the raw secret confined
//! to the server crate's request-handling path and out of the wider repo
//! layer that may grow listings, joins, etc.

use sqlx::PgPool;

use crate::model::ApiTokenRow;

/// Fields required to insert a new token. `created_at` is `DEFAULT now()`
/// and `last_used_at` starts NULL, so neither is part of this struct.
#[derive(Clone)]
pub struct NewApiToken {
    pub name: String,
    /// SHA-256 hex of the raw token. The raw token must never reach this
    /// layer — only its hash. See `server::token::hash`.
    pub token_hash: String,
}

/// Custom `Debug` redacts `token_hash` to mirror [`ApiTokenRow`]'s policy.
impl std::fmt::Debug for NewApiToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewApiToken")
            .field("name", &self.name)
            .field("token_hash", &"<redacted>")
            .finish()
    }
}

pub async fn insert(pool: &PgPool, new: NewApiToken) -> sqlx::Result<ApiTokenRow> {
    sqlx::query_as!(
        ApiTokenRow,
        r#"
        INSERT INTO api_token (name, token_hash)
        VALUES ($1, $2)
        RETURNING id, name, token_hash, last_used_at, created_at
        "#,
        new.name,
        new.token_hash,
    )
    .fetch_one(pool)
    .await
}

/// Look up a token row by its hash. Used by the auth middleware to decide
/// whether a Bearer token is valid. Returns `None` when the hash is not
/// known (i.e. the token is invalid or has been revoked).
///
/// The hash is high-entropy (SHA-256 of a 256-bit random value) so direct
/// equality lookup via the `UNIQUE` index is fine — timing-attack mitigation
/// at the DB level is unnecessary for tokens with this much entropy.
pub async fn find_by_hash(pool: &PgPool, token_hash: &str) -> sqlx::Result<Option<ApiTokenRow>> {
    sqlx::query_as!(
        ApiTokenRow,
        r#"
        SELECT id, name, token_hash, last_used_at, created_at
        FROM api_token
        WHERE token_hash = $1
        "#,
        token_hash,
    )
    .fetch_optional(pool)
    .await
}

/// Stamp `last_used_at = now()`. The auth middleware calls this on every
/// successful request as a best-effort write — failures must not block the
/// actual request, so the caller is expected to log and swallow errors.
pub async fn mark_used(pool: &PgPool, id: i64) -> sqlx::Result<()> {
    sqlx::query!(
        r#"UPDATE api_token SET last_used_at = now() WHERE id = $1"#,
        id,
    )
    .execute(pool)
    .await
    .map(|_| ())
}

/// List all tokens. Used by `sakurasato token list` to print a one-line
/// summary per token (`name` / `created_at` / `last_used_at`). Hash is included
/// in the row but the CLI never prints it.
pub async fn list_all(pool: &PgPool) -> sqlx::Result<Vec<ApiTokenRow>> {
    sqlx::query_as!(
        ApiTokenRow,
        r#"
        SELECT id, name, token_hash, last_used_at, created_at
        FROM api_token
        ORDER BY created_at ASC
        "#,
    )
    .fetch_all(pool)
    .await
}

/// Hard-delete a token by id. Used by `sakurasato token revoke --id N`.
/// Returns `true` if a row was deleted, `false` if the id was not found.
pub async fn delete_by_id(pool: &PgPool, id: i64) -> sqlx::Result<bool> {
    let res = sqlx::query!(r#"DELETE FROM api_token WHERE id = $1"#, id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}
