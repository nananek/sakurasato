//! Compile-time checked queries against the `note` table.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sqlx::PgPool;
use sqlx::types::Json;

use crate::model::{NoteRow, Visibility};

#[derive(Debug, Clone)]
pub struct NewNote {
    pub ap_id: String,
    pub actor_id: i64,
    pub content: String,
    pub language: Option<String>,
    pub in_reply_to_ap_id: Option<String>,
    pub in_reply_to_note_id: Option<i64>,
    pub summary: Option<String>,
    pub visibility: Visibility,
    pub sensitive: bool,
    pub to_recipients: Vec<String>,
    pub cc_recipients: Vec<String>,
    pub attachments: JsonValue,
    pub tags: JsonValue,
    pub is_local: bool,
    pub url: Option<String>,
    pub published_at: DateTime<Utc>,
}

pub async fn insert<'e, E>(executor: E, new: NewNote) -> sqlx::Result<NoteRow>
where
    E: sqlx::PgExecutor<'e>,
{
    let to_recipients_json =
        serde_json::to_value(&new.to_recipients).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
    let cc_recipients_json =
        serde_json::to_value(&new.cc_recipients).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
    sqlx::query_as!(
        NoteRow,
        r#"
        INSERT INTO note (
            ap_id, actor_id, content, language, in_reply_to_ap_id,
            in_reply_to_note_id, summary, visibility, sensitive,
            to_recipients, cc_recipients, attachments, tags, is_local,
            url, published_at
        )
        VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
            $11, $12, $13, $14, $15, $16
        )
        RETURNING
            id, ap_id, actor_id, content, language, in_reply_to_ap_id,
            in_reply_to_note_id, summary, visibility, sensitive,
            to_recipients as "to_recipients: Json<Vec<String>>",
            cc_recipients as "cc_recipients: Json<Vec<String>>",
            attachments as "attachments: Json<JsonValue>",
            tags as "tags: Json<JsonValue>",
            is_local, url, published_at, created_at, updated_at
        "#,
        new.ap_id,
        new.actor_id,
        new.content,
        new.language,
        new.in_reply_to_ap_id,
        new.in_reply_to_note_id,
        new.summary,
        new.visibility.as_str(),
        new.sensitive,
        to_recipients_json,
        cc_recipients_json,
        new.attachments,
        new.tags,
        new.is_local,
        new.url,
        new.published_at,
    )
    .fetch_one(executor)
    .await
}

/// Insert 後に `ap_id` と `url` を「実 id を埋めた canonical URL」に書き
/// 直すヘルパ。POST /api/v1/notes で `note.id` 採番後にしか canonical URL
/// が決まらない (= `https://<host>/notes/{id}`) ため、insert → update の
/// 2 段で生成する。
///
/// 同一トランザクションで呼ぶ前提なので executor を取る。
pub async fn set_ap_id_and_url<'e, E>(
    executor: E,
    id: i64,
    ap_id: &str,
    url: &str,
) -> sqlx::Result<()>
where
    E: sqlx::PgExecutor<'e>,
{
    sqlx::query!(
        "UPDATE note SET ap_id = $1, url = $2, updated_at = now() WHERE id = $3",
        ap_id,
        url,
        id,
    )
    .execute(executor)
    .await
    .map(|_| ())
}

pub async fn get_by_ap_id(pool: &PgPool, ap_id: &str) -> sqlx::Result<Option<NoteRow>> {
    sqlx::query_as!(
        NoteRow,
        r#"
        SELECT
            id, ap_id, actor_id, content, language, in_reply_to_ap_id,
            in_reply_to_note_id, summary, visibility, sensitive,
            to_recipients as "to_recipients: Json<Vec<String>>",
            cc_recipients as "cc_recipients: Json<Vec<String>>",
            attachments as "attachments: Json<JsonValue>",
            tags as "tags: Json<JsonValue>",
            is_local, url, published_at, created_at, updated_at
        FROM note WHERE ap_id = $1
        "#,
        ap_id,
    )
    .fetch_optional(pool)
    .await
}

/// Home timeline 用に Note 行 + actor の表示情報を join して返す。
///
/// 対象 = `viewer_actor_id` 本人の投稿、または `viewer_actor_id` が
/// state='accepted' で follow している actor の投稿。`direct` だけは外す
/// (本人宛 DM の閲覧は別 API で扱う予定 ── M? 以降)。
///
/// 件数は `limit`、カーソルは `before_id` (= `note.id` を opaque な
/// `i64` 整数として扱う)。`before_id = None` の場合は最新から `limit` 件。
/// 結果は `id DESC` 順 (= 作成順 / `BIGSERIAL` の自然な単調列に依存)。
/// `published_at` ではなく `id` で並べることで、リモートから到着した
/// 古い投稿を新しい順 (= 我々が受信した順) で表示できる。
#[allow(clippy::similar_names)]
pub async fn list_home_timeline(
    pool: &PgPool,
    viewer_actor_id: i64,
    before_id: Option<i64>,
    limit: i64,
) -> sqlx::Result<Vec<TimelineEntry>> {
    sqlx::query_as!(
        TimelineEntry,
        r#"
        SELECT
            n.id, n.ap_id, n.actor_id, n.content, n.language, n.in_reply_to_ap_id,
            n.in_reply_to_note_id, n.summary, n.visibility, n.sensitive,
            n.to_recipients as "to_recipients: Json<Vec<String>>",
            n.cc_recipients as "cc_recipients: Json<Vec<String>>",
            n.attachments as "attachments: Json<JsonValue>",
            n.tags as "tags: Json<JsonValue>",
            n.is_local, n.url, n.published_at, n.created_at, n.updated_at,
            a.ap_id AS actor_ap_id,
            a.preferred_username AS actor_preferred_username,
            a.display_name AS actor_display_name
        FROM note n
        JOIN actor a ON a.id = n.actor_id
        WHERE
            n.visibility <> 'direct'
            AND (
                n.actor_id = $1
                OR n.actor_id IN (
                    SELECT followed_actor_id
                    FROM follow
                    WHERE follower_actor_id = $1 AND state = 'accepted'
                )
            )
            AND ($2::BIGINT IS NULL OR n.id < $2)
        ORDER BY n.id DESC
        LIMIT $3
        "#,
        viewer_actor_id,
        before_id,
        limit,
    )
    .fetch_all(pool)
    .await
}

/// `list_home_timeline` の戻り行。Note の通常カラムに加え、actor 表示
/// 情報を join 同行に持つ。
#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct TimelineEntry {
    pub id: i64,
    pub ap_id: String,
    pub actor_id: i64,
    pub content: String,
    pub language: Option<String>,
    pub in_reply_to_ap_id: Option<String>,
    pub in_reply_to_note_id: Option<i64>,
    pub summary: Option<String>,
    pub visibility: String,
    pub sensitive: bool,
    pub to_recipients: Json<Vec<String>>,
    pub cc_recipients: Json<Vec<String>>,
    pub attachments: Json<JsonValue>,
    pub tags: Json<JsonValue>,
    pub is_local: bool,
    pub url: Option<String>,
    pub published_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub actor_ap_id: String,
    pub actor_preferred_username: String,
    pub actor_display_name: Option<String>,
}

pub async fn get_by_id(pool: &PgPool, id: i64) -> sqlx::Result<Option<NoteRow>> {
    sqlx::query_as!(
        NoteRow,
        r#"
        SELECT
            id, ap_id, actor_id, content, language, in_reply_to_ap_id,
            in_reply_to_note_id, summary, visibility, sensitive,
            to_recipients as "to_recipients: Json<Vec<String>>",
            cc_recipients as "cc_recipients: Json<Vec<String>>",
            attachments as "attachments: Json<JsonValue>",
            tags as "tags: Json<JsonValue>",
            is_local, url, published_at, created_at, updated_at
        FROM note WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await
}
