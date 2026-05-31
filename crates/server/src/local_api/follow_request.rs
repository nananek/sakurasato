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
use tracing::{error, warn};

use crate::follow_request::{MutateError, approve_or_reject};
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct PendingFollow {
    pub id: i64,
    pub ap_id: String,
    pub follower_ap_id: String,
    pub received_at: String,
    /// 行の現状の `follow.state`。`?state=pending` (default) 経路では常に
    /// `"pending"`、`?state=all` 経路では `"accepted"` / `"rejected"` も入る。
    pub state: String,
}

#[derive(Debug, Serialize)]
pub struct ListResponse {
    pub items: Vec<PendingFollow>,
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct ListQuery {
    /// `pending` (default) ── 承認待ちのみ。
    /// `all`                ── 全 state (= テスト再実行時の cleanup 用)。
    pub state: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct MutateResponse {
    pub id: i64,
    pub new_state: &'static str,
}

pub async fn list(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<ListQuery>,
) -> Response {
    match repo::follow::list_for_local(state.pool(), q.state.as_deref()).await {
        Ok(rows) => {
            let items = rows
                .into_iter()
                .map(
                    |(id, ap_id, follower_ap_id, st, created_at)| PendingFollow {
                        id,
                        ap_id,
                        follower_ap_id,
                        received_at: created_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                        state: st,
                    },
                )
                .collect();
            (StatusCode::OK, Json(ListResponse { items })).into_response()
        }
        Err(err) => {
            error!(?err, "GET /api/v1/follow-requests: query failed");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

/// **PR #80 round-2 #6 (test re-runnability)**: 古いフォロー行をハード
/// 削除する。Undo Follow ハンドラ未実装の現状、管理者 / pytest 連合テストが
/// 「Bob → me の accepted 行を消して新規 Follow からやり直す」用途で使う。
pub async fn delete(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    match repo::follow::delete_by_id(state.pool(), id).await {
        Ok(0) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("no follow row with id={id}") })),
        )
            .into_response(),
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => {
            error!(?err, id, "DELETE /api/v1/follow-requests/{id}: failed");
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
        // **PR #80 round-2 #3 fix**: MutateError variant ごとに HTTP status を
        // 出し分ける。`Infra` は DB / 配送先 URL parse 失敗等の **再試行で
        // 解決しうるサーバ側障害** なので 503 + body はサニタイズ済みの
        // 固定文言 (= sqlx エラーのテーブル名 / クエリ断片が外に漏れない)。
        // それ以外 (`NotFound` / `NotPending` / `NotForLocal`) はクライアントが
        // 修正可能な論理エラーで 400、本文は短い英文だけ返す。
        Err(MutateError::Infra(err)) => {
            error!(?err, id, ?new_state, "follow-request mutate: infra failure");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": "follow-request mutation failed; check server logs"
                })),
            )
                .into_response()
        }
        Err(err @ MutateError::NotFound(_)) => {
            warn!(error = %err, id, "follow-request mutate: not found");
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": err.to_string() })),
            )
                .into_response()
        }
        Err(err @ (MutateError::NotPending { .. } | MutateError::NotForLocal { .. })) => {
            warn!(error = %err, id, ?new_state, "follow-request mutate: client error");
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": err.to_string() })),
            )
                .into_response()
        }
    }
}
