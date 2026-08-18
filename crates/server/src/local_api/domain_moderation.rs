//! PR4 (計画書 §6.6): `GET /api/v1/domains` / `GET /api/v1/domains/{host}` /
//! `POST /api/v1/domains/{host}/silence` / `POST /api/v1/domains/{host}/suspend` /
//! `DELETE /api/v1/domains/{host}`。
//!
//! TUI のドメイン管理画面 (§6.7) が叩く経路。CLI (`sakurasato-server domain`)
//! と同じ core ロジック ([`crate::domain_moderation`]) を共有する。
//!
//! 計画書 §6.6 は `GET /api/v1/domains/{host}` と
//! `GET /api/v1/domains/{host}/followers` を別エンドポイントとして提案して
//! いるが、単一ユーザーサーバでは 1 ドメインあたりのフォロー件数が少数に
//! 留まる想定のため、詳細エンドポイント 1 本に following/followers 一覧を
//! 含めて返す簡略化を採用する (TUI の詳細画面は 1 回の GET で完結する)。

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use sakurasato_core::model::ActorRow;
use sakurasato_core::repo;
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use crate::domain_moderation::{
    DomainModerationError, detail_core, silence_core, suspend_core, unset_core,
};
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct DomainSummaryDto {
    pub host: String,
    pub actor_count: i64,
    pub severity: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DomainListResponse {
    pub domains: Vec<DomainSummaryDto>,
}

/// `GET /api/v1/domains` ── 既知ドメイン一覧 + 統計 + 現在の moderation state。
pub async fn list(State(state): State<AppState>) -> Response {
    match repo::domain_moderation::list_known_hosts(state.pool()).await {
        Ok(rows) => {
            let domains = rows
                .into_iter()
                .map(|r| DomainSummaryDto {
                    host: r.host,
                    actor_count: r.actor_count,
                    severity: r.severity,
                })
                .collect();
            Json(DomainListResponse { domains }).into_response()
        }
        Err(err) => {
            error!(?err, "list_known_hosts query failed");
            service_unavailable("domain list query failed; check server logs")
        }
    }
}

#[derive(Debug, Serialize)]
pub struct DomainFollowEntry {
    pub follow_id: i64,
    pub follow_state: String,
    pub follow_created_at: chrono::DateTime<chrono::Utc>,
    pub actor: ActorRow,
}

#[derive(Debug, Serialize)]
pub struct DomainDetailResponse {
    pub host: String,
    pub severity: Option<String>,
    pub reason: Option<String>,
    pub known_actor_count: i64,
    pub accepted_following_count: i64,
    pub accepted_followers_count: i64,
    pub pending_following_count: i64,
    pub pending_followers_count: i64,
    pub following: Vec<DomainFollowEntry>,
    pub followers: Vec<DomainFollowEntry>,
}

/// `GET /api/v1/domains/{host}` ── 統計 + moderation state + フォロー一覧。
pub async fn detail(State(state): State<AppState>, Path(host): Path<String>) -> Response {
    match detail_core(&state, &host).await {
        Ok(d) => {
            let following = d
                .following
                .into_iter()
                .map(|f| DomainFollowEntry {
                    follow_id: f.follow_id,
                    follow_state: f.follow_state,
                    follow_created_at: f.follow_created_at,
                    actor: f.actor,
                })
                .collect();
            let followers = d
                .followers
                .into_iter()
                .map(|f| DomainFollowEntry {
                    follow_id: f.follow_id,
                    follow_state: f.follow_state,
                    follow_created_at: f.follow_created_at,
                    actor: f.actor,
                })
                .collect();
            Json(DomainDetailResponse {
                host: d.host,
                severity: d.moderation.as_ref().map(|m| m.severity.clone()),
                reason: d.moderation.as_ref().and_then(|m| m.reason.clone()),
                known_actor_count: d.stats.known_actor_count,
                accepted_following_count: d.stats.accepted_following_count,
                accepted_followers_count: d.stats.accepted_followers_count,
                pending_following_count: d.stats.pending_following_count,
                pending_followers_count: d.stats.pending_followers_count,
                following,
                followers,
            })
            .into_response()
        }
        Err(err) => map_error(&err, "detail"),
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct ModerateRequest {
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SilenceResponse {
    pub host: String,
    pub severity: String,
}

/// `POST /api/v1/domains/{host}/silence`。
pub async fn silence(
    State(state): State<AppState>,
    Path(host): Path<String>,
    Json(req): Json<ModerateRequest>,
) -> Response {
    match silence_core(&state, &host, req.reason).await {
        Ok(row) => {
            info!(host = %row.host, "POST /api/v1/domains/{host}/silence ok");
            Json(SilenceResponse {
                host: row.host,
                severity: row.severity,
            })
            .into_response()
        }
        Err(err) => map_error(&err, "silence"),
    }
}

#[derive(Debug, Serialize)]
pub struct SuspendResponse {
    pub host: String,
    pub severity: String,
    pub forced_unfollow_count: u64,
}

/// `POST /api/v1/domains/{host}/suspend`。破壊的操作 (双方向フォロー強制
/// 解除) だが、TUI 側で確認オーバーレイ (PR7、§6.7) を挟んだ後にここを叩く
/// 前提で、API 自体には確認機構を持たせない (CLI と同じ即時実行方針)。
pub async fn suspend(
    State(state): State<AppState>,
    Path(host): Path<String>,
    Json(req): Json<ModerateRequest>,
) -> Response {
    match suspend_core(&state, &host, req.reason).await {
        Ok(outcome) => {
            info!(
                host = %outcome.row.host,
                forced_unfollow_count = outcome.forced_unfollow_count,
                "POST /api/v1/domains/{host}/suspend ok"
            );
            Json(SuspendResponse {
                host: outcome.row.host,
                severity: outcome.row.severity,
                forced_unfollow_count: outcome.forced_unfollow_count,
            })
            .into_response()
        }
        Err(err) => map_error(&err, "suspend"),
    }
}

/// `DELETE /api/v1/domains/{host}` ── 措置解除。
pub async fn unset(State(state): State<AppState>, Path(host): Path<String>) -> Response {
    match unset_core(&state, &host).await {
        Ok(()) => {
            info!(host = %host, "DELETE /api/v1/domains/{host} ok");
            StatusCode::NO_CONTENT.into_response()
        }
        Err(err) => map_error(&err, "unset"),
    }
}

fn map_error(err: &DomainModerationError, op: &str) -> Response {
    match err {
        DomainModerationError::BadRequest(msg) => {
            warn!(error = %msg, op, "/api/v1/domains: bad request");
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": msg })),
            )
                .into_response()
        }
        DomainModerationError::NotFound(msg) => {
            warn!(error = %msg, op, "/api/v1/domains: not found");
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": msg })),
            )
                .into_response()
        }
        DomainModerationError::Unavailable(msg) => {
            warn!(error = %msg, op, "/api/v1/domains: unavailable");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "error": msg })),
            )
                .into_response()
        }
        DomainModerationError::Internal(e) => {
            error!(error = ?e, op, "/api/v1/domains: internal failure");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": "domain moderation operation failed; check server logs"
                })),
            )
                .into_response()
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
