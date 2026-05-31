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
use serde_json::json;
use tracing::{error, warn};

use crate::local_api::media::build_media_url;
use crate::state::AppState;

const LIMIT_DEFAULT: i64 = 40;
const LIMIT_MAX: i64 = 80;

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
    fn from_entry_with_reactions(e: TimelineEntry, reactions: Vec<ReactionSummaryDto>) -> Self {
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
        }
    }
}

/// `ReactionSummaryRow` (DB) → `ReactionSummaryDto` (API)。
///
/// `image_key` のうち local emoji (`emoji/local/<shortcode>.webp`) は
/// `build_media_url` で `https://<host>/media/<key>` に展開し、TUI が
/// `/media/proxy?url=...` 越しに fetch できるようにする。remote emoji
/// (= 元 URL の絶対 URL) はそのまま渡す。
fn row_to_dto(host: &str, row: ReactionSummaryRow) -> ReactionSummaryDto {
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
            TimelineNote::from_entry_with_reactions(e, reactions)
        })
        .collect();

    Json(TimelineResponse {
        notes,
        next_before_id,
    })
    .into_response()
}

fn clamp_limit(req: Option<i64>) -> i64 {
    let l = req.unwrap_or(LIMIT_DEFAULT);
    l.clamp(1, LIMIT_MAX)
}

fn error_with_body(status: StatusCode, reason: &str) -> Response {
    (status, Json(json!({"error": reason}))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
