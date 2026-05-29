//! Compile-time checked queries against the `emoji` table.
//!
//! Misskey zip import (M8) inserts/overwrites local emojis here. The UNIQUE
//! `(shortcode, host)` constraint backs the upsert pattern used below.

use sqlx::PgPool;
use sqlx::types::Json;

use crate::model::EmojiRow;

/// True if `shortcode` is safe to use as an emoji identifier and as a
/// component of the versitygw object key (e.g. `emoji/local/<shortcode>.webp`).
///
/// Restricted to ASCII alphanumeric + `_` + `-`, length 1..=64. Matches the
/// `CHECK` constraint in `0005_emoji.sql` so the application catches the
/// failure before hitting the DB (better error message) but the DB also
/// refuses it as defence in depth (zip-slip relative to S3 keys).
pub fn is_valid_shortcode(shortcode: &str) -> bool {
    let len = shortcode.len();
    (1..=64).contains(&len)
        && shortcode
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[derive(Debug, Clone)]
pub struct NewLocalEmoji {
    pub shortcode: String,
    pub category: Option<String>,
    pub aliases: Vec<String>,
    pub image_key: String,
    pub media_type: String,
}

/// Insert a local custom emoji, overwriting any prior entry with the same
/// shortcode (Misskey import semantics: 「同名は上書き」, CLAUDE.md §5.4).
pub async fn upsert_local(pool: &PgPool, new: NewLocalEmoji) -> sqlx::Result<EmojiRow> {
    if !is_valid_shortcode(&new.shortcode) {
        return Err(sqlx::Error::Protocol(format!(
            "invalid emoji shortcode {:?}; must match [a-zA-Z0-9_-]{{1,64}}",
            new.shortcode
        )));
    }
    let aliases_json =
        serde_json::to_value(&new.aliases).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
    sqlx::query_as!(
        EmojiRow,
        r#"
        INSERT INTO emoji (shortcode, host, category, aliases, image_key, media_type, is_local)
        VALUES ($1, NULL, $2, $3, $4, $5, TRUE)
        ON CONFLICT (shortcode, host) DO UPDATE SET
            category = EXCLUDED.category,
            aliases = EXCLUDED.aliases,
            image_key = EXCLUDED.image_key,
            media_type = EXCLUDED.media_type,
            updated_at = now()
        RETURNING
            id, shortcode, host, category,
            aliases as "aliases: Json<Vec<String>>",
            image_key, media_type, ap_id, is_local, created_at, updated_at
        "#,
        new.shortcode,
        new.category,
        aliases_json,
        new.image_key,
        new.media_type,
    )
    .fetch_one(pool)
    .await
}

pub async fn get_local_by_shortcode(
    pool: &PgPool,
    shortcode: &str,
) -> sqlx::Result<Option<EmojiRow>> {
    sqlx::query_as!(
        EmojiRow,
        r#"
        SELECT
            id, shortcode, host, category,
            aliases as "aliases: Json<Vec<String>>",
            image_key, media_type, ap_id, is_local, created_at, updated_at
        FROM emoji WHERE shortcode = $1 AND host IS NULL
        "#,
        shortcode,
    )
    .fetch_optional(pool)
    .await
}
