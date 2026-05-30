//! `GET /api/v1/whoami` — 認証成功確認 + ローカル actor の最小サマリ。
//!
//! TUI は接続直後にここを叩いてトークン有効性を確認する。秘密鍵は返さない。
//! local actor が見つからない (= `sakurasato init` 未実行) 場合は 404。

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use sakurasato_core::repo;
use serde::Serialize;

use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct WhoamiResponse {
    pub ap_id: String,
    pub preferred_username: String,
    pub host: String,
    pub display_name: Option<String>,
    pub summary: Option<String>,
    pub icon_url: Option<String>,
    pub image_url: Option<String>,
    pub inbox: String,
    pub outbox: Option<String>,
}

pub async fn handle(State(state): State<AppState>) -> Response {
    let host = &state.config().server.host;
    let user = &state.config().server.user;
    let row = match repo::actor::get_by_username_host(state.pool(), user, host).await {
        Ok(Some(row)) if row.is_local => row,
        Ok(_) => return StatusCode::NOT_FOUND.into_response(),
        Err(err) => {
            tracing::error!(?err, "whoami actor lookup failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    Json(WhoamiResponse {
        ap_id: row.ap_id,
        preferred_username: row.preferred_username,
        host: row.host,
        display_name: row.display_name,
        summary: row.summary,
        icon_url: row.icon_url,
        image_url: row.image_url,
        inbox: row.inbox_url,
        outbox: row.outbox_url,
    })
    .into_response()
}
