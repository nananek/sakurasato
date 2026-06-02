//! `GET /api/v1/timeline/home` ── ホームタイムライン取得。
//!
//! 自分の投稿 + accepted follow している remote actor の投稿を、
//! `note.id` 降順 (= 新しい順) で返す。
//!
//! ## クエリパラメータ
//!
//! - `limit` (任意、既定 40、上限 80) ── 1 回で返す件数。
//! - `before_id` (任意) ── 与えると `id < before_id` の行だけ返す。
//!   TUI が「もっと読む」を実装するための単純カーソル。
//!
//! ## エラー
//!
//! - 503: ローカル actor 未 init / DB アクセス失敗。
//!   フォロー先 actor が居ない初期状態でも 200 + 空配列を返す (自分の投稿が
//!   無くてもエラーにしない)。

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use sakurasato_core::repo;
use sakurasato_core::repo::note::TimelineEntry;
use sakurasato_core::repo::reaction::ReactionSummaryRow;
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use tracing::{error, warn};

use crate::local_api::media::build_media_url;
use crate::state::AppState;

pub(crate) const LIMIT_DEFAULT: i64 = 40;
pub(crate) const LIMIT_MAX: i64 = 80;

#[derive(Debug, Deserialize)]
pub struct TimelineQuery {
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub before_id: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct TimelineNote {
    pub id: i64,
    pub ap_id: String,
    pub url: Option<String>,
    pub actor_id: i64,
    pub actor_ap_id: String,
    pub actor_preferred_username: String,
    pub actor_display_name: Option<String>,
    /// 投稿主のアバター URL。M5 PR2 で TUI 側が画像表示に使う。
    /// クライアントが直接 fetch する想定 (server は decode しない)。
    pub actor_icon_url: Option<String>,
    pub content: String,
    pub summary: Option<String>,
    pub language: Option<String>,
    pub visibility: String,
    pub sensitive: bool,
    pub in_reply_to_ap_id: Option<String>,
    pub in_reply_to_note_id: Option<i64>,
    pub published_at: chrono::DateTime<chrono::Utc>,
    pub is_local: bool,
    /// M8 PR3: 受領したリアクション集計 (`content` 単位)。空 Vec は省略しない
    /// (= 必ず `reactions: []` を返す) ── 既存 TUI の serde は配列 default が
    /// `Vec::new()` で安全。
    #[serde(default)]
    pub reactions: Vec<ReactionSummaryDto>,
    /// Issue #133 (4): 添付メディア。AP `Document` を扱いやすい形に正規化
    /// した一覧。Timeline の `📎 N` バッジ件数と、Note 詳細モーダルの
    /// プレビューに使う。空 Vec は省略しない (= 必ず `attachments: []`)。
    #[serde(default)]
    pub attachments: Vec<AttachmentDto>,
    /// Issue #133 (5): 本文の `:shortcode:` に対応する Emoji tag 一覧
    /// (AP `tag` のうち `type == "Emoji"` だけ抜き出した形)。詳細モーダルで
    /// shortcode と画像のギャラリー表示に使う。空 Vec は省略しない。
    #[serde(default)]
    pub emojis: Vec<EmojiDto>,
}

/// Note 添付の TUI 向け正規化形式。AP `Document` / `Image` のフィールドの
/// うち TUI が実描画に使う部分だけを引き出す。
///
/// - `url`: 表示用 URL (= `/media/proxy?url=...` 経由で fetch する元 URL)。
///   ローカル添付は `https://<host>/media/<key>`、リモートは送られてきた URL。
/// - `media_type`: `image/webp` 等。`image/` で始まらない (= 動画など) なら
///   TUI は preview をスキップしてリンクだけ出す。
/// - `alt`: AP `name` 由来の代替テキスト。なければ `None`。
/// - `width` / `height`: 元 Document の寸法 (= AP では任意)。preview のアスペクト比に。
#[derive(Debug, Serialize)]
pub struct AttachmentDto {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

/// Note 本文中で参照される custom emoji の最小情報。`shortcode` は AP
/// `name` で `:foo:` (ローカル) または `:foo@host:` (リモート) の形を維持。
///
/// `image_url` は媒体取得用 ── ローカル emoji は `/media/emoji/local/...`、
/// リモートは AP `icon.url` を素のまま渡す (= TUI 側は `media/proxy?url=`
/// 経由で fetch)。
#[derive(Debug, Serialize)]
pub struct EmojiDto {
    pub shortcode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    /// `Some(true)` = ローカル絵文字 (= `/media/proxy?url=` 経由で OK)、
    /// `Some(false)` = リモート、`None` = 由来不明 (= `image_url` の host を
    /// 自インスタンスと比較する)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_local: Option<bool>,
}

/// `TimelineNote.reactions` の 1 要素。`content` は AP のまま (`:foo:` /
/// Unicode / `:foo@host:`)。`emoji_image_url` は local emoji の場合
/// `/media/emoji/local/...` の絶対 URL、remote emoji の場合は連合先サーバ
/// の URL がそのまま入る (TUI は `media/proxy?url=...&variant=emoji` 経由で
/// fetch する想定)。
#[derive(Debug, Serialize)]
pub struct ReactionSummaryDto {
    pub content: String,
    pub count: i64,
    #[serde(default)]
    pub emoji_image_url: Option<String>,
    #[serde(default)]
    pub emoji_media_type: Option<String>,
    /// `Some(true)` = local emoji (= 直接 `image_url` を fetch してよい)、
    /// `Some(false)` = remote emoji (= TUI 側で proxy 経由)、`None` = Unicode。
    #[serde(default)]
    pub emoji_is_local: Option<bool>,
}

impl TimelineNote {
    pub(crate) fn from_entry_with_reactions(
        e: TimelineEntry,
        reactions: Vec<ReactionSummaryDto>,
        host: &str,
    ) -> Self {
        let attachments = parse_attachments(&e.attachments);
        let emojis = parse_emojis(&e.tags, host);
        Self {
            id: e.id,
            ap_id: e.ap_id,
            url: e.url,
            actor_id: e.actor_id,
            actor_ap_id: e.actor_ap_id,
            actor_preferred_username: e.actor_preferred_username,
            actor_display_name: e.actor_display_name,
            actor_icon_url: e.actor_icon_url,
            content: e.content,
            summary: e.summary,
            language: e.language,
            visibility: e.visibility,
            sensitive: e.sensitive,
            in_reply_to_ap_id: e.in_reply_to_ap_id,
            in_reply_to_note_id: e.in_reply_to_note_id,
            published_at: e.published_at,
            is_local: e.is_local,
            reactions,
            attachments,
            emojis,
        }
    }
}

/// `note.attachments` JSONB を [`AttachmentDto`] の Vec に正規化する。AP
/// `Document` / `Image` / `Audio` / `Video` を `url` / `mediaType` / `name`
/// / `width` / `height` だけ抜く。`url` を持たない要素は無視する。
pub(crate) fn parse_attachments(raw: &JsonValue) -> Vec<AttachmentDto> {
    let JsonValue::Array(arr) = raw else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|v| {
            let url = v.get("url").and_then(JsonValue::as_str)?.to_string();
            let media_type = v
                .get("mediaType")
                .and_then(JsonValue::as_str)
                .map(str::to_string);
            let alt = v
                .get("name")
                .and_then(JsonValue::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let width = v.get("width").and_then(JsonValue::as_u64).and_then(|w| {
                if w > u64::from(u32::MAX) {
                    None
                } else {
                    u32::try_from(w).ok()
                }
            });
            let height = v.get("height").and_then(JsonValue::as_u64).and_then(|h| {
                if h > u64::from(u32::MAX) {
                    None
                } else {
                    u32::try_from(h).ok()
                }
            });
            Some(AttachmentDto {
                url,
                media_type,
                alt,
                width,
                height,
            })
        })
        .collect()
}

/// `note.tags` JSONB を走査し `type == "Emoji"` の要素だけ [`EmojiDto`] に
/// 変換する。AP `Emoji` は `name` (shortcode) と `icon.url` を持つ。
///
/// `is_local` は `image_url` の host を `local_host` (= 自インスタンス) と
/// 比較して決める。AP の `Emoji` 自体には `is_local` フィールドが無いため
/// host 比較が現状唯一の信号。
pub(crate) fn parse_emojis(raw: &JsonValue, local_host: &str) -> Vec<EmojiDto> {
    let JsonValue::Array(arr) = raw else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|v| {
            if v.get("type").and_then(JsonValue::as_str) != Some("Emoji") {
                return None;
            }
            let shortcode = v
                .get("name")
                .and_then(JsonValue::as_str)
                .filter(|s| !s.is_empty())?
                .to_string();
            let icon = v.get("icon");
            let image_url = icon
                .and_then(|i| i.get("url"))
                .and_then(JsonValue::as_str)
                .map(str::to_string);
            let media_type = icon
                .and_then(|i| i.get("mediaType"))
                .and_then(JsonValue::as_str)
                .map(str::to_string);
            let is_local = image_url
                .as_deref()
                .and_then(|u| url::Url::parse(u).ok())
                .and_then(|p| p.host_str().map(str::to_ascii_lowercase))
                .map(|h| h == local_host.to_ascii_lowercase());
            Some(EmojiDto {
                shortcode,
                image_url,
                media_type,
                is_local,
            })
        })
        .collect()
}

/// `ReactionSummaryRow` (DB) → `ReactionSummaryDto` (API)。
///
/// `image_key` のうち local emoji (`emoji/local/<shortcode>.webp`) は
/// `build_media_url` で `https://<host>/media/<key>` に展開し、TUI が
/// `/media/proxy?url=...` 越しに fetch できるようにする。remote emoji
/// (= 元 URL の絶対 URL) はそのまま渡す。
pub(crate) fn row_to_dto(host: &str, row: ReactionSummaryRow) -> ReactionSummaryDto {
    let emoji_image_url = match (row.is_local, row.image_key.as_ref()) {
        (Some(true), Some(key)) => Some(build_media_url(host, key)),
        (Some(false), Some(key)) => Some(key.clone()),
        _ => None,
    };
    ReactionSummaryDto {
        content: row.content,
        count: row.count,
        emoji_image_url,
        emoji_media_type: row.media_type,
        emoji_is_local: row.is_local,
    }
}

#[derive(Debug, Serialize)]
pub struct TimelineResponse {
    pub notes: Vec<TimelineNote>,
    /// 次ページを取るときに使う `before_id` (= 最後の note の id)。
    /// `notes` が空のときは `None`。
    pub next_before_id: Option<i64>,
}

pub async fn home(State(state): State<AppState>, Query(q): Query<TimelineQuery>) -> Response {
    let host = &state.config().server.host;
    let user = &state.config().server.user;

    let actor = match repo::actor::get_by_username_host(state.pool(), user, host).await {
        Ok(Some(a)) if a.is_local => a,
        Ok(_) => {
            return error_with_body(
                StatusCode::SERVICE_UNAVAILABLE,
                "local actor not initialized; run `sakurasato init`",
            );
        }
        Err(err) => {
            error!(?err, "timeline/home: local actor lookup failed");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };

    let limit = clamp_limit(q.limit);
    let entries =
        match repo::note::list_home_timeline(state.pool(), actor.id, q.before_id, limit).await {
            Ok(rows) => rows,
            Err(err) => {
                error!(?err, "timeline/home: list_home_timeline failed");
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
        };

    let next_before_id = entries.last().map(|e| e.id);

    // M8 PR3: 当ページの note 全件のリアクションを 1 クエリで集計する。
    // failure は warn でログに残し、空の集計で続行 ── タイムライン本体を
    // 失敗させたくない。
    let note_ids: Vec<i64> = entries.iter().map(|e| e.id).collect();
    let mut by_note: HashMap<i64, Vec<ReactionSummaryDto>> = HashMap::new();
    match repo::reaction::counts_for_notes(state.pool(), &note_ids).await {
        Ok(rows) => {
            for row in rows {
                by_note
                    .entry(row.note_id)
                    .or_default()
                    .push(row_to_dto(host, row));
            }
        }
        Err(err) => {
            warn!(?err, "timeline/home: reaction counts_for_notes failed");
        }
    }

    let notes: Vec<TimelineNote> = entries
        .into_iter()
        .map(|e| {
            let reactions = by_note.remove(&e.id).unwrap_or_default();
            TimelineNote::from_entry_with_reactions(e, reactions, host)
        })
        .collect();

    Json(TimelineResponse {
        notes,
        next_before_id,
    })
    .into_response()
}

pub(crate) fn clamp_limit(req: Option<i64>) -> i64 {
    let l = req.unwrap_or(LIMIT_DEFAULT);
    l.clamp(1, LIMIT_MAX)
}

fn error_with_body(status: StatusCode, reason: &str) -> Response {
    (status, Json(json!({"error": reason}))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn clamp_limit_uses_default_when_missing() {
        assert_eq!(clamp_limit(None), LIMIT_DEFAULT);
    }

    #[test]
    fn clamp_limit_caps_at_max() {
        assert_eq!(clamp_limit(Some(1000)), LIMIT_MAX);
    }

    #[test]
    fn clamp_limit_floors_at_one() {
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(-5)), 1);
    }

    #[test]
    fn parse_attachments_extracts_fields() {
        let raw = json!([
            {
                "type": "Document",
                "mediaType": "image/webp",
                "url": "https://e.example/m/1.webp",
                "name": "alt text",
                "width": 800,
                "height": 600,
            },
            {
                "type": "Image",
                "mediaType": "image/jpeg",
                "url": "https://e.example/m/2.jpg",
            },
        ]);
        let out = parse_attachments(&raw);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].url, "https://e.example/m/1.webp");
        assert_eq!(out[0].media_type.as_deref(), Some("image/webp"));
        assert_eq!(out[0].alt.as_deref(), Some("alt text"));
        assert_eq!(out[0].width, Some(800));
        assert_eq!(out[0].height, Some(600));
        assert_eq!(out[1].url, "https://e.example/m/2.jpg");
        assert!(out[1].alt.is_none());
        assert!(out[1].width.is_none());
    }

    #[test]
    fn parse_attachments_skips_no_url() {
        // `url` 無しの entry は drop ── 表示できないため。
        let raw = json!([
            { "type": "Document", "mediaType": "image/webp" },
            { "type": "Document", "url": "https://e.example/ok.webp" },
        ]);
        let out = parse_attachments(&raw);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].url, "https://e.example/ok.webp");
    }

    #[test]
    fn parse_attachments_empty_alt_dropped() {
        let raw = json!([{ "type": "Document", "url": "u", "name": "" }]);
        let out = parse_attachments(&raw);
        assert!(out[0].alt.is_none());
    }

    #[test]
    fn parse_attachments_non_array_returns_empty() {
        assert!(parse_attachments(&JsonValue::Null).is_empty());
        assert!(parse_attachments(&json!({"key": "val"})).is_empty());
    }

    #[test]
    fn parse_emojis_filters_type_emoji_only() {
        let raw = json!([
            {
                "type": "Emoji",
                "name": ":blob:",
                "icon": {"url": "https://local.test/media/emoji/local/blob.webp", "mediaType": "image/webp"}
            },
            {
                "type": "Mention",
                "name": "@alice@e.example",
                "href": "https://e.example/users/alice"
            },
            {
                "type": "Hashtag",
                "name": "#tag",
                "href": "https://e.example/tags/tag"
            },
            {
                "type": "Emoji",
                "name": ":remote@misskey.io:",
                "icon": {"url": "https://misskey.io/files/x.webp", "mediaType": "image/webp"}
            },
        ]);
        let out = parse_emojis(&raw, "local.test");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].shortcode, ":blob:");
        assert_eq!(out[0].is_local, Some(true));
        assert_eq!(out[1].shortcode, ":remote@misskey.io:");
        assert_eq!(out[1].is_local, Some(false));
    }

    #[test]
    fn parse_emojis_missing_icon_url_drops_image() {
        let raw = json!([
            { "type": "Emoji", "name": ":foo:" },
            { "type": "Emoji", "name": ":bar:", "icon": {} },
        ]);
        let out = parse_emojis(&raw, "local.test");
        assert_eq!(out.len(), 2);
        assert!(out[0].image_url.is_none());
        assert!(out[1].image_url.is_none());
        // `image_url` が無いので `is_local` 判定不能 → `None`。
        assert!(out[0].is_local.is_none());
    }

    #[test]
    fn parse_emojis_empty_name_dropped() {
        let raw = json!([{ "type": "Emoji", "name": "" }]);
        let out = parse_emojis(&raw, "local.test");
        assert!(out.is_empty());
    }

    #[test]
    fn parse_emojis_non_array_returns_empty() {
        assert!(parse_emojis(&JsonValue::Null, "local.test").is_empty());
        assert!(parse_emojis(&json!({"x": 1}), "local.test").is_empty());
    }
}
