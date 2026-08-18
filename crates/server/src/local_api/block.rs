//! PR2 (計画書 §5.7): `POST /api/v1/block` / `DELETE /api/v1/block/{id}` /
//! `GET /api/v1/blocks`。
//!
//! TUI Profile 画面のブロック操作と `:block @acct` コマンドが叩く経路。
//! CLI (`sakurasato-server block`) と同じ core ロジック
//! ([`crate::block::create_block_core`] / [`crate::block::delete_block_core`])
//! を共有する ── `local_api/follow.rs` と対称の構成。
//!
//! ## ルート
//!
//! - `POST /api/v1/block` ── body は `{"acct": "..."}` / `{"actor_uri": "..."}`
//!   / `{"actor_id": 123}` のいずれか 1 つ。双方向フォロー強制解除の後
//!   `Block` activity を配送する。
//! - `DELETE /api/v1/block/{block_id}` ── 本人のブロックのみ削除可能
//!   (403 ガード)。`Undo{Block}` を配送する。フォロー関係は自動復活しない。
//! - `GET /api/v1/blocks` ── ブロック中の actor 一覧 (TUI 管理画面用)。

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use sakurasato_core::model::ActorRow;
use sakurasato_core::repo;
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use crate::block::{BlockError, BlockOutcome, UnblockOutcome, create_block_core, delete_block_core};
use crate::follow::FollowTarget;
use crate::state::AppState;

/// `POST /api/v1/block` の入力 body。`local_api/follow.rs::CreateFollowRequest`
/// と同じく 3 方式は排他 (複数指定は 400)。
#[derive(Debug, Deserialize)]
pub struct CreateBlockRequest {
    #[serde(default)]
    pub acct: Option<String>,
    #[serde(default)]
    pub actor_uri: Option<String>,
    #[serde(default)]
    pub actor_id: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct CreateBlockResponse {
    pub block_id: i64,
    pub ap_id: String,
    pub target_actor_id: i64,
    pub target_ap_id: String,
    pub delivery_queue_id: i64,
    pub inbox_url: String,
}

#[derive(Debug, Serialize)]
pub struct DeleteBlockResponse {
    pub block_id: i64,
    pub target_ap_id: String,
    pub delivery_queue_id: i64,
    pub inbox_url: String,
}

#[derive(Debug, Serialize)]
pub struct BlockListEntry {
    pub block_id: i64,
    pub block_created_at: chrono::DateTime<chrono::Utc>,
    pub actor: ActorRow,
}

#[derive(Debug, Serialize)]
pub struct BlockListResponse {
    pub entries: Vec<BlockListEntry>,
}

pub async fn create(
    State(state): State<AppState>,
    Json(req): Json<CreateBlockRequest>,
) -> Response {
    let target = match parse_target(&req) {
        Ok(t) => t,
        Err(msg) => return bad_request(msg),
    };
    match create_block_core(&state, target).await {
        Ok(outcome) => {
            info!(
                block_id = outcome.block.id,
                target = %outcome.target.ap_id,
                "POST /api/v1/block ok"
            );
            (StatusCode::OK, Json(to_create_response(outcome))).into_response()
        }
        Err(err) => map_block_error(&err, "create"),
    }
}

pub async fn delete(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    match delete_block_core(&state, id).await {
        Ok(outcome) => {
            info!(
                block_id = outcome.block_id,
                target = %outcome.target_ap_id,
                queue_id = outcome.queue_id,
                "DELETE /api/v1/block ok"
            );
            (StatusCode::OK, Json(to_delete_response(outcome))).into_response()
        }
        Err(err) => map_block_error(&err, "delete"),
    }
}

/// `GET /api/v1/blocks` ── ブロック中の actor 一覧。
pub async fn list(State(state): State<AppState>) -> Response {
    let local = match resolve_local_actor(&state).await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let rows = match repo::block::list_blocked_by_local(state.pool(), local.id).await {
        Ok(rows) => rows,
        Err(err) => {
            error!(?err, local_id = local.id, "list_blocked_by_local query failed");
            return service_unavailable("block list query failed; check server logs");
        }
    };
    let entries = rows
        .into_iter()
        .map(|r| BlockListEntry {
            block_id: r.block_id,
            block_created_at: r.block_created_at,
            actor: r.actor,
        })
        .collect();
    Json(BlockListResponse { entries }).into_response()
}

fn parse_target(req: &CreateBlockRequest) -> Result<FollowTarget, String> {
    let provided = [
        req.acct.is_some(),
        req.actor_uri.is_some(),
        req.actor_id.is_some(),
    ]
    .iter()
    .filter(|b| **b)
    .count();
    match provided {
        0 => Err("provide exactly one of `acct` / `actor_uri` / `actor_id`".into()),
        1 => {
            if let Some(acct) = req.acct.as_deref() {
                Ok(FollowTarget::Acct(acct.to_string()))
            } else if let Some(uri) = req.actor_uri.as_deref() {
                Ok(FollowTarget::ActorUri(uri.to_string()))
            } else if let Some(id) = req.actor_id {
                Ok(FollowTarget::ActorId(id))
            } else {
                Err("internal: target parser missed a case".into())
            }
        }
        _ => Err("provide only one of `acct` / `actor_uri` / `actor_id`".into()),
    }
}

fn to_create_response(outcome: BlockOutcome) -> CreateBlockResponse {
    CreateBlockResponse {
        block_id: outcome.block.id,
        ap_id: outcome.block.ap_id.clone(),
        target_actor_id: outcome.target.id,
        target_ap_id: outcome.target.ap_id.clone(),
        delivery_queue_id: outcome.queue_id,
        inbox_url: outcome.inbox_url,
    }
}

fn to_delete_response(outcome: UnblockOutcome) -> DeleteBlockResponse {
    DeleteBlockResponse {
        block_id: outcome.block_id,
        target_ap_id: outcome.target_ap_id,
        delivery_queue_id: outcome.queue_id,
        inbox_url: outcome.inbox_url,
    }
}

/// `BlockError` を HTTP status + JSON body にマップする。
/// `local_api/follow.rs::map_follow_error` と対称。
fn map_block_error(err: &BlockError, op: &str) -> Response {
    match err {
        BlockError::BadRequest(msg) => {
            warn!(error = %msg, op, "/api/v1/block: bad request");
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": msg })),
            )
                .into_response()
        }
        BlockError::BadGateway(msg) => {
            warn!(error = %msg, op, "/api/v1/block: upstream failure");
            (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({ "error": msg })),
            )
                .into_response()
        }
        BlockError::NotFound(msg) => {
            warn!(error = %msg, op, "/api/v1/block: not found");
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": msg })),
            )
                .into_response()
        }
        BlockError::Unavailable(msg) => {
            warn!(error = %msg, op, "/api/v1/block: unavailable");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "error": msg })),
            )
                .into_response()
        }
        BlockError::Conflict(msg) => {
            warn!(error = %msg, op, "/api/v1/block: conflict");
            (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": msg })),
            )
                .into_response()
        }
        BlockError::Forbidden(msg) => {
            warn!(error = %msg, op, "/api/v1/block: forbidden");
            (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({ "error": msg })),
            )
                .into_response()
        }
        BlockError::Internal(e) => {
            error!(error = ?e, op, "/api/v1/block: internal failure");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": "block operation failed; check server logs"
                })),
            )
                .into_response()
        }
    }
}

async fn resolve_local_actor(state: &AppState) -> Result<ActorRow, Response> {
    let host = &state.config().server.host;
    let user = &state.config().server.user;
    match repo::actor::get_by_username_host(state.pool(), user, host).await {
        Ok(Some(a)) if a.is_local => Ok(a),
        Ok(_) => Err(service_unavailable(
            "local actor not initialized; run `sakurasato-server init` first",
        )),
        Err(err) => {
            error!(?err, "local actor lookup failed");
            Err(service_unavailable(
                "block list operation failed; check server logs",
            ))
        }
    }
}

fn service_unavailable(msg: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({ "error": msg })),
    )
        .into_response()
}

fn bad_request(msg: impl Into<String>) -> Response {
    let msg = msg.into();
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": msg })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_target_requires_exactly_one() {
        assert!(
            parse_target(&CreateBlockRequest {
                acct: None,
                actor_uri: None,
                actor_id: None,
            })
            .is_err()
        );
        assert!(
            parse_target(&CreateBlockRequest {
                acct: Some("alice@example.test".into()),
                actor_uri: Some("https://example.test/users/alice".into()),
                actor_id: None,
            })
            .is_err()
        );
    }

    #[test]
    fn parse_target_accepts_actor_id() {
        let t = parse_target(&CreateBlockRequest {
            acct: None,
            actor_uri: None,
            actor_id: Some(42),
        })
        .unwrap();
        assert!(matches!(t, FollowTarget::ActorId(42)));
    }
}
