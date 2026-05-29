//! Compile-time checked queries against the `note` table.

use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;
use sqlx::PgPool;
use sqlx::types::Json;

use crate::model::NoteRow;

#[derive(Debug, Clone)]
pub struct NewNote {
    pub ap_id: String,
    pub actor_id: i64,
    pub content: String,
    pub language: Option<String>,
    pub in_reply_to_ap_id: Option<String>,
    pub in_reply_to_note_id: Option<i64>,
    pub summary: Option<String>,
    pub visibility: String,
    pub sensitive: bool,
    pub to_recipients: Vec<String>,
    pub cc_recipients: Vec<String>,
    pub attachments: JsonValue,
    pub tags: JsonValue,
    pub is_local: bool,
    pub url: Option<String>,
    pub published_at: DateTime<Utc>,
}

pub async fn insert(pool: &PgPool, new: NewNote) -> sqlx::Result<NoteRow> {
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
        new.visibility,
        new.sensitive,
        to_recipients_json,
        cc_recipients_json,
        new.attachments,
        new.tags,
        new.is_local,
        new.url,
        new.published_at,
    )
    .fetch_one(pool)
    .await
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
