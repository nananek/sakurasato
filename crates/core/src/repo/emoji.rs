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

/// Local emoji を shortcode 前方一致で検索する。TUI のサジェスト UI
/// (Issue #101) が `:foo` まで打った段階で叩く。
///
/// - 前方一致は ASCII-lowercase 比較 (shortcode は元から ASCII)。
/// - `limit` は呼び出し側で 1..=100 にクランプ済みの想定。負値は 0 件扱い。
/// - 結果は shortcode ASC でソート (= 安定した popup 表示)。
pub async fn list_local_by_prefix(
    pool: &PgPool,
    prefix: &str,
    limit: i64,
) -> sqlx::Result<Vec<EmojiRow>> {
    let pat = format!("{}%", prefix.to_ascii_lowercase());
    sqlx::query_as!(
        EmojiRow,
        r#"
        SELECT
            id, shortcode, host, category,
            aliases as "aliases: Json<Vec<String>>",
            image_key, media_type, ap_id, is_local, created_at, updated_at
        FROM emoji
        WHERE host IS NULL
          AND lower(shortcode) LIKE $1
        ORDER BY shortcode ASC
        LIMIT $2
        "#,
        pat,
        limit,
    )
    .fetch_all(pool)
    .await
}

/// Remote custom emoji の upsert 入力 (M8 PR2)。
///
/// `image_url` は連合先サーバの実 URL (= `Emoji.icon.url`)。本 PR では
/// 取得・キャッシュは行わず URL のまま `image_key` に格納する ── M9 で
/// media-proxy 経由のキャッシュに切り替える想定 (CLAUDE.md §5.4)。
#[derive(Debug, Clone)]
pub struct NewRemoteEmoji {
    pub shortcode: String,
    /// `Emoji.id` (= AP URI)。`name` だけだと衝突しうるので一意キーは `ap_id`。
    pub ap_id: String,
    /// `Emoji.id` のホスト。`null` は不可 (= remote はホスト必須)。
    pub host: String,
    pub image_url: String,
    pub media_type: String,
}

/// Remote emoji を upsert する。`ap_id` をキーに同じ AP URI なら同じ行を
/// 返す ── 同じ remote 絵文字を別 reaction で何度学習しても 1 行で済む。
pub async fn upsert_remote(pool: &PgPool, new: NewRemoteEmoji) -> sqlx::Result<EmojiRow> {
    if !is_valid_shortcode(&new.shortcode) {
        return Err(sqlx::Error::Protocol(format!(
            "invalid emoji shortcode {:?}; must match [a-zA-Z0-9_-]{{1,64}}",
            new.shortcode
        )));
    }
    if new.host.is_empty() {
        return Err(sqlx::Error::Protocol(
            "remote emoji host must not be empty".into(),
        ));
    }
    sqlx::query_as!(
        EmojiRow,
        r#"
        INSERT INTO emoji (shortcode, host, category, aliases, image_key, media_type, ap_id, is_local)
        VALUES ($1, $2, NULL, '[]'::jsonb, $3, $4, $5, FALSE)
        ON CONFLICT (ap_id) DO UPDATE SET
            shortcode = EXCLUDED.shortcode,
            host = EXCLUDED.host,
            image_key = EXCLUDED.image_key,
            media_type = EXCLUDED.media_type,
            updated_at = now()
        RETURNING
            id, shortcode, host, category,
            aliases as "aliases: Json<Vec<String>>",
            image_key, media_type, ap_id, is_local, created_at, updated_at
        "#,
        new.shortcode,
        new.host,
        new.image_url,
        new.media_type,
        new.ap_id,
    )
    .fetch_one(pool)
    .await
}

/// `id` で 1 行引く。`reaction.emoji_id` を Activity 再構築する経路で使う
/// (例: Undo の `object` を inline 化するときに元の `Emoji` tag を組み立てる)。
pub async fn get_by_id(pool: &PgPool, id: i64) -> sqlx::Result<Option<EmojiRow>> {
    sqlx::query_as!(
        EmojiRow,
        r#"
        SELECT
            id, shortcode, host, category,
            aliases as "aliases: Json<Vec<String>>",
            image_key, media_type, ap_id, is_local, created_at, updated_at
        FROM emoji WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await
}

/// `ap_id` で 1 行引く。Remote emoji の存在チェック用。
pub async fn get_by_ap_id(pool: &PgPool, ap_id: &str) -> sqlx::Result<Option<EmojiRow>> {
    sqlx::query_as!(
        EmojiRow,
        r#"
        SELECT
            id, shortcode, host, category,
            aliases as "aliases: Json<Vec<String>>",
            image_key, media_type, ap_id, is_local, created_at, updated_at
        FROM emoji WHERE ap_id = $1
        "#,
        ap_id,
    )
    .fetch_optional(pool)
    .await
}
