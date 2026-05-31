//! Issue #66 / M12 ── 鍵アカ運用の承認待ち follow を local API で操作する。
//!
//! - `GET    /api/v1/follow-requests`               ── pending 一覧
//! - `POST   /api/v1/follow-requests/{id}/approve`  ── Accept 配送 + state 遷移
//! - `POST   /api/v1/follow-requests/{id}/reject`   ── Reject 配送 + state 遷移
//!
//! CLI (`sakurasato-server follow-request …`) と同じロジックを薄く包む。
//! pytest 連合テスト・TUI 双方からここを叩く想定。

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use sakurasato_core::model::FollowState;
use sakurasato_core::repo;
use serde::Serialize;
use tracing::error;

use crate::follow_request::approve_or_reject;
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct PendingFollow {
    pub id: i64,
    pub ap_id: String,
    pub follower_ap_id: String,
    pub received_at: String,
}

#[derive(Debug, Serialize)]
pub struct ListResponse {
    pub items: Vec<PendingFollow>,
}

#[derive(Debug, Serialize)]
pub struct MutateResponse {
    pub id: i64,
    pub new_state: &'static str,
}

pub async fn list(State(state): State<AppState>) -> Response {
    match repo::follow::list_pending_for_local(state.pool()).await {
        Ok(rows) => {
            let items = rows
                .into_iter()
                .map(|(id, ap_id, follower_ap_id, created_at)| PendingFollow {
                    id,
                    ap_id,
                    follower_ap_id,
                    received_at: created_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                })
                .collect();
            (StatusCode::OK, Json(ListResponse { items })).into_response()
        }
        Err(err) => {
            error!(?err, "GET /api/v1/follow-requests: query failed");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

pub async fn approve(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    mutate(&state, id, FollowState::Accepted).await
}

pub async fn reject(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    mutate(&state, id, FollowState::Rejected).await
}

async fn mutate(state: &AppState, id: i64, new_state: FollowState) -> Response {
    match approve_or_reject(state, id, new_state).await {
        Ok(()) => (
            StatusCode::OK,
            Json(MutateResponse {
                id,
                new_state: new_state.as_str(),
            }),
        )
            .into_response(),
        Err(err) => {
            // CLI と同じく「pending 以外を mutate」「missing row」等は client
            // エラー的だが、ここでは serialize する verbose な型を作るより
            // 400 + JSON エラーで簡潔に返す。message は本番で泣くようなものは
            // 入らない (= follow_id とプロトコル状態程度)。
            let msg = format!("{err}");
            error!(error = %msg, id, ?new_state, "follow-request mutate failed");
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": msg })),
            )
                .into_response()
        }
    }
}
