//! `GET /api/v1/notifications` / `POST /api/v1/notifications/mark-all-read`
//! (= 通知 #206 PR3、TUI 通知ビュー用)。
//!
//! in-app 通知フィード (migration 0020 `notification`) を TUI が一覧・既読化する
//! 経路。MiAuth (Aria) 用の `/api/i/notifications` (#206 PR2) とは別 listener
//! (= local API、UDS + Bearer) で、DTO は TUI が描画しやすい平坦な形にする
//! (notifier の acct / 表示名と note 本文プレビューを展開済みで返す)。

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use sakurasato_core::model::NotificationRow;
use sakurasato_core::repo;
use serde::{Deserialize, Serialize};
use tracing::error;

use crate::state::AppState;

const LIMIT_DEFAULT: i64 = 40;
const LIMIT_MAX: i64 = 80;
/// note 本文プレビューの最大文字数 (= TUI の 1 行表示用)。
const PREVIEW_MAX_CHARS: usize = 80;

#[derive(Debug, Default, Deserialize)]
pub struct ListQuery {
    pub limit: Option<i64>,
    /// 排他上限 (= `id < until_id`、古い方向へのページング)。
    pub until_id: Option<i64>,
}

/// TUI 描画用に展開済みの通知 1 件。
#[derive(Debug, Serialize)]
pub struct NotificationItem {
    pub id: i64,
    /// `NotificationEvent::as_str()` の値 (= `reaction` / `follow` / `mention` 等)。
    pub event_type: String,
    pub is_read: bool,
    pub created_at: String,
    /// 通知を起こした相手の acct (`user` or `user@host`)。purge 済みなら `None`。
    pub notifier_acct: Option<String>,
    pub notifier_display_name: Option<String>,
    pub note_id: Option<i64>,
    /// 対象 note 本文の plain text プレビュー (HTML 除去 + 短縮)。
    pub note_preview: Option<String>,
    /// reaction の内容 (`reaction` event のみ)。
    pub reaction: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ListResponse {
    pub items: Vec<NotificationItem>,
    pub unread_count: i64,
}

pub async fn list(State(state): State<AppState>, Query(q): Query<ListQuery>) -> Response {
    let Some(recipient) = resolve_local_actor_id(&state).await else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let limit = q.limit.unwrap_or(LIMIT_DEFAULT).clamp(1, LIMIT_MAX);

    let rows =
        match repo::notification::list(state.pool(), recipient, limit, None, q.until_id).await {
            Ok(v) => v,
            Err(err) => {
                error!(?err, "GET /api/v1/notifications: query failed");
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
        };
    let unread_count = repo::notification::count_unread(state.pool(), recipient)
        .await
        .unwrap_or(0);

    let mut items = Vec::with_capacity(rows.len());
    for row in rows {
        items.push(build_item(&state, row).await);
    }
    (
        StatusCode::OK,
        Json(ListResponse {
            items,
            unread_count,
        }),
    )
        .into_response()
}

pub async fn mark_all_read(State(state): State<AppState>) -> Response {
    let Some(recipient) = resolve_local_actor_id(&state).await else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match repo::notification::mark_all_read(state.pool(), recipient).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => {
            error!(?err, "POST /api/v1/notifications/mark-all-read: failed");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

async fn build_item(state: &AppState, row: NotificationRow) -> NotificationItem {
    let (notifier_acct, notifier_display_name) = match row.notifier_actor_id {
        Some(notifier_id) => match repo::actor::get_by_id(state.pool(), notifier_id).await {
            Ok(Some(actor)) => {
                let acct = if actor.is_local {
                    actor.preferred_username.clone()
                } else {
                    format!("{}@{}", actor.preferred_username, actor.host)
                };
                (Some(acct), actor.display_name.clone())
            }
            _ => (None, None),
        },
        None => (None, None),
    };

    let note_preview = match row.note_id {
        Some(note_id) => match repo::note::get_by_id(state.pool(), note_id).await {
            Ok(Some(note)) => Some(preview(&note.content)),
            _ => None,
        },
        None => None,
    };

    NotificationItem {
        id: row.id,
        event_type: row.event_type,
        is_read: row.is_read,
        created_at: row
            .created_at
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        notifier_acct,
        notifier_display_name,
        note_id: row.note_id,
        note_preview,
        reaction: row.reaction,
    }
}

/// note 本文 (AP `content` = HTML) を plain text 化して 1 行プレビューに短縮する。
fn preview(content: &str) -> String {
    let plain = crate::miauth::text::html_to_plain_text(content);
    let one_line = plain.replace('\n', " ");
    let trimmed = one_line.trim();
    if trimmed.chars().count() > PREVIEW_MAX_CHARS {
        let s: String = trimmed.chars().take(PREVIEW_MAX_CHARS).collect();
        format!("{s}…")
    } else {
        trimmed.to_string()
    }
}

/// local actor (= recipient) の id。お一人様サーバ前提で 1 件。
async fn resolve_local_actor_id(state: &AppState) -> Option<i64> {
    let host = &state.config().server.host;
    let user = &state.config().server.user;
    match repo::actor::get_by_username_host(state.pool(), user, host).await {
        Ok(Some(row)) if row.is_local => Some(row.id),
        _ => None,
    }
}
