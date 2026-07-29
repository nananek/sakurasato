//! Compile-time checked queries against the `emoji` table.
//!
//! Misskey zip import (M8) inserts/overwrites local emojis here. The UNIQUE
//! `(shortcode, host)` constraint backs the upsert pattern used below.

use sqlx::PgPool;
use sqlx::types::Json;

use crate::model::EmojiRow;

/// ローカル emoji を一覧する API (公開 `/api/emojis` / `/api/v1/custom_emojis`、
/// `MiAuth` `/api/emojis`) が 1 度に取る最大件数。お一人様サーバの emoji 数を十分
/// 上回る値。複数 endpoint で重複定義しないよう core に置く。
pub const LIST_FETCH_LIMIT: i64 = 10_000;

/// True if `shortcode` is safe to use as an emoji identifier and as a
/// component of the versitygw object key (e.g. `emoji/local/<shortcode>.webp`).
///
/// Restricted to ASCII alphanumeric + `_` + `-`, length 1..=128. Matches the
/// `CHECK` constraint in `0017_emoji_shortcode_128.sql` (which superseded the
/// `{1,64}` constraint in `0005_emoji.sql`) so the application catches the
/// failure before hitting the DB (better error message) but the DB also
/// refuses it as defence in depth (zip-slip relative to S3 keys).
///
/// Issue #188: upper bound widened from 64 to 128 to align with Misskey
/// (`^[a-zA-Z0-9_]+$` length 128). The charset stays `[a-zA-Z0-9_-]` (= still
/// allowing `-` even though Misskey itself does not) to preserve existing rows
/// with hyphenated shortcodes (e.g. `blob-smile`) imported from Misskey-
/// compatible zip packs.
pub fn is_valid_shortcode(shortcode: &str) -> bool {
    let len = shortcode.len();
    (1..=128).contains(&len)
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
    /// AP `_misskey_license.freeText` 相当。Misskey zip import で取り込む。`None` 可。
    pub license: Option<String>,
    /// Misskey `isSensitive`。
    pub is_sensitive: bool,
}

/// Insert a local custom emoji, overwriting any prior entry with the same
/// shortcode (Misskey import semantics: 「同名は上書き」, CLAUDE.md §5.4).
pub async fn upsert_local(pool: &PgPool, new: NewLocalEmoji) -> sqlx::Result<EmojiRow> {
    if !is_valid_shortcode(&new.shortcode) {
        return Err(sqlx::Error::Protocol(format!(
            "invalid emoji shortcode {:?}; must match [a-zA-Z0-9_-]{{1,128}}",
            new.shortcode
        )));
    }
    let aliases_json =
        serde_json::to_value(&new.aliases).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
    sqlx::query_as!(
        EmojiRow,
        r#"
        INSERT INTO emoji (shortcode, host, category, aliases, image_key, media_type, is_local, license, is_sensitive)
        VALUES ($1, NULL, $2, $3, $4, $5, TRUE, $6, $7)
        ON CONFLICT (shortcode, host) DO UPDATE SET
            category = EXCLUDED.category,
            aliases = EXCLUDED.aliases,
            image_key = EXCLUDED.image_key,
            media_type = EXCLUDED.media_type,
            license = EXCLUDED.license,
            is_sensitive = EXCLUDED.is_sensitive,
            updated_at = now()
        RETURNING
            id, shortcode, host, category,
            aliases as "aliases: Json<Vec<String>>",
            image_key, media_type, ap_id, is_local, license, is_sensitive, created_at, updated_at, last_failed_at
        "#,
        new.shortcode,
        new.category,
        aliases_json,
        new.image_key,
        new.media_type,
        new.license,
        new.is_sensitive,
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
            image_key, media_type, ap_id, is_local, license, is_sensitive, created_at, updated_at, last_failed_at
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
/// - `limit` は呼び出し側で 1..=`MAX_LIMIT` にクランプ済みの想定。負値は 0 件扱い。
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
            image_key, media_type, ap_id, is_local, license, is_sensitive, created_at, updated_at, last_failed_at
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

/// LIKE/`ILIKE` pattern 内の特殊文字 (`\`, `%`, `_`) を `\` でエスケープする。
///
/// SQL 側で `ESCAPE '\'` を指定して使う。`is_valid_shortcode` の文字集合に
/// 載らない `%` 等を TUI 検索バッファに打たれても、全件マッチ等の劣化挙動に
/// ならないようにする。
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Local emoji を shortcode / aliases の部分一致で検索する (Issue #130)。
///
/// 部分一致は `ILIKE %query% ESCAPE '\'` で大文字小文字を無視する。aliases は
/// JSONB の `text[]` 要素を `jsonb_array_elements_text` で展開して各要素に
/// 同じパターンを当てる。
///
/// - `query` が空 (trim 後) なら [`list_local_by_prefix`] にフォールバックする
///   (= 既存挙動: 全件を shortcode ASC で `limit` 件返す)。
/// - `limit` は呼び出し側で 1..=`MAX_LIMIT` にクランプ済みの想定。
/// - 結果は shortcode ASC でソート。サーバ側で前方一致を上位に並べる重み付けは
///   行わない ── TUI 側 `recompute()` で再ソートする責務分離。
pub async fn search_local_by_substring(
    pool: &PgPool,
    query: &str,
    limit: i64,
) -> sqlx::Result<Vec<EmojiRow>> {
    let q = query.trim();
    if q.is_empty() {
        return list_local_by_prefix(pool, "", limit).await;
    }
    let pat = format!("%{}%", escape_like(q));
    sqlx::query_as!(
        EmojiRow,
        r#"
        SELECT
            id, shortcode, host, category,
            aliases as "aliases: Json<Vec<String>>",
            image_key, media_type, ap_id, is_local, license, is_sensitive, created_at, updated_at, last_failed_at
        FROM emoji
        WHERE host IS NULL
          AND (
              shortcode ILIKE $1 ESCAPE '\'
              OR EXISTS (
                  SELECT 1 FROM jsonb_array_elements_text(aliases) AS a
                  WHERE a ILIKE $1 ESCAPE '\'
              )
          )
        ORDER BY shortcode ASC
        LIMIT $2
        "#,
        pat,
        limit,
    )
    .fetch_all(pool)
    .await
}

/// Remote custom emoji の upsert 入力 (M8 PR #45 → Issue #135 / #192 で改修)。
///
/// - `image_key`: **versitygw 上のローカルキャッシュキー**
///   (`emoji/remote/<host>/<shortcode>.webp`) または `None`。Issue #192 以降、
///   `None` は **「今回の試行は失敗、ただし既存 `image_key` は温存」** を意味する
///   (= 旧 URL row が `NULL` に降格しないよう SQL 側で `COALESCE`)。
/// - `last_failed_at`: 直近の fetch 失敗時刻。fetch 成功時は `None` を渡して
///   reset、失敗時は `Some(Utc::now())` を渡す。
///
/// 既存 row との挙動マトリクス (`upsert_remote` 内 SQL の `ON CONFLICT` 句):
///
/// | 経路 | 入力 `image_key` | 既存 `image_key` | 書き戻し | `last_failed_at` |
/// |---|---|---|---|---|
/// | 新規 success | `Some(key)` | (なし、`INSERT`) | `INSERT Some(key)` | `NULL` |
/// | 新規 failure | `None`     | (なし、`INSERT`) | `INSERT NULL`      | `now()` |
/// | 既存 success | `Some(key)` | 任意           | `UPDATE Some(key)` | `NULL` |
/// | 既存 failure | `None`     | `Some(prev)`   | **温存 `Some(prev)`** | `now()` |
/// | 既存 failure | `None`     | `None`         | `UPDATE NULL`      | `now()` |
///
/// `media_type` は `image_key` を書き戻す経路でのみ追従する (= 失敗時は据え置き、
/// 旧 URL row の `media_type` を巻き戻さない)。
#[derive(Debug, Clone)]
pub struct NewRemoteEmoji {
    pub shortcode: String,
    /// `Emoji.id` (= AP URI)。`name` だけだと衝突しうるので一意キーは `ap_id`。
    pub ap_id: String,
    /// `Emoji.id` のホスト。`null` は不可 (= remote はホスト必須)。
    pub host: String,
    /// versitygw 上のキャッシュキー、または `None` (= 今回失敗、既存値温存)。
    pub image_key: Option<String>,
    pub media_type: String,
    /// fetch を試みて失敗した時刻。成功時は `None` (= reset)。Issue #192。
    pub last_failed_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Remote emoji を upsert する。`ap_id` をキーに同じ AP URI なら同じ行を
/// 返す ── 同じ remote 絵文字を別 reaction で何度学習しても 1 行で済む。
///
/// Issue #192: `image_key` を `COALESCE(EXCLUDED.image_key, emoji.image_key)`
/// で書き戻す ── fetch 失敗時に既存値を温存し、旧 URL row が NULL に降格
/// しないようにする。`media_type` は `image_key` が新規に来た時のみ追従。
pub async fn upsert_remote(pool: &PgPool, new: NewRemoteEmoji) -> sqlx::Result<EmojiRow> {
    if !is_valid_shortcode(&new.shortcode) {
        return Err(sqlx::Error::Protocol(format!(
            "invalid emoji shortcode {:?}; must match [a-zA-Z0-9_-]{{1,128}}",
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
        INSERT INTO emoji (shortcode, host, category, aliases, image_key, media_type, ap_id, is_local, last_failed_at)
        VALUES ($1, $2, NULL, '[]'::jsonb, $3, $4, $5, FALSE, $6)
        ON CONFLICT (ap_id) DO UPDATE SET
            shortcode = EXCLUDED.shortcode,
            host = EXCLUDED.host,
            image_key = COALESCE(EXCLUDED.image_key, emoji.image_key),
            media_type = CASE
                WHEN EXCLUDED.image_key IS NOT NULL THEN EXCLUDED.media_type
                ELSE emoji.media_type
            END,
            last_failed_at = EXCLUDED.last_failed_at,
            updated_at = now()
        RETURNING
            id, shortcode, host, category,
            aliases as "aliases: Json<Vec<String>>",
            image_key, media_type, ap_id, is_local, license, is_sensitive, created_at, updated_at, last_failed_at
        "#,
        new.shortcode,
        new.host,
        new.image_key,
        new.media_type,
        new.ap_id,
        new.last_failed_at,
    )
    .fetch_one(pool)
    .await
}

/// versitygw にキャッシュ済み (`image_key IS NOT NULL`) のリモート emoji を
/// host, shortcode 順で列挙する。TUI の絵文字管理画面「リモート絵文字を
/// ローカルにコピー」機能 (Issue #328 系) が対象を絞り込む母集団。
///
/// `image_key IS NULL` (= fetch 失敗キャッシュ、`should_skip_fetch` 参照) は
/// コピー元になれないためここで除外する ── 呼び出し側が毎回 filter する
/// 手間を省く。
pub async fn list_remote_cached(pool: &PgPool, limit: i64) -> sqlx::Result<Vec<EmojiRow>> {
    sqlx::query_as!(
        EmojiRow,
        r#"
        SELECT
            id, shortcode, host, category,
            aliases as "aliases: Json<Vec<String>>",
            image_key, media_type, ap_id, is_local, license, is_sensitive, created_at, updated_at, last_failed_at
        FROM emoji
        WHERE host IS NOT NULL AND image_key IS NOT NULL
        ORDER BY host ASC, shortcode ASC
        LIMIT $1
        "#,
        limit,
    )
    .fetch_all(pool)
    .await
}

/// リモート emoji を shortcode / host の部分一致で検索する。
/// [`search_local_by_substring`] のリモート版 (`escape_like` 共有)。
///
/// `query` が空なら [`list_remote_cached`] にフォールバックする。
pub async fn search_remote_cached(
    pool: &PgPool,
    query: &str,
    limit: i64,
) -> sqlx::Result<Vec<EmojiRow>> {
    let q = query.trim();
    if q.is_empty() {
        return list_remote_cached(pool, limit).await;
    }
    let pat = format!("%{}%", escape_like(q));
    sqlx::query_as!(
        EmojiRow,
        r#"
        SELECT
            id, shortcode, host, category,
            aliases as "aliases: Json<Vec<String>>",
            image_key, media_type, ap_id, is_local, license, is_sensitive, created_at, updated_at, last_failed_at
        FROM emoji
        WHERE host IS NOT NULL AND image_key IS NOT NULL
          AND (
              shortcode ILIKE $1 ESCAPE '\'
              OR host ILIKE $1 ESCAPE '\'
          )
        ORDER BY host ASC, shortcode ASC
        LIMIT $2
        "#,
        pat,
        limit,
    )
    .fetch_all(pool)
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
            image_key, media_type, ap_id, is_local, license, is_sensitive, created_at, updated_at, last_failed_at
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
            image_key, media_type, ap_id, is_local, license, is_sensitive, created_at, updated_at, last_failed_at
        FROM emoji WHERE ap_id = $1
        "#,
        ap_id,
    )
    .fetch_optional(pool)
    .await
}
