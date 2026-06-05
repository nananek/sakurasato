//! `POST /api/following/create` / `POST /api/following/delete` (= M14 #160, 親 issue #150)。
//!
//! Misskey 互換 ── ローカル user が指定 actor を follow / unfollow する経路。
//!
//! ## wire 仕様 (clean-room)
//!
//! - <https://api-doc.misskey.io/api/endpoints/following/create>
//! - <https://api-doc.misskey.io/api/endpoints/following/delete>
//!
//! observed wire shape:
//!
//! - body: `{ i: <token>, userId: "<string>" }`
//! - 成功時: 相手 user の `MissUser` (Misskey の慣行 ── follow 状態を含む `UserDetailed`
//!   オブジェクトを返す。ここでは [`crate::miauth::conv::from_actor_detailed`] を
//!   流用し、`isFollowing` / `isFollowed` 等の relationship フィールドは別 endpoint
//!   `users/relation` を別途実装する想定で本 PR では含めない)
//! - エラー時: `404 NO_SUCH_USER` (= 不在 actor) / `409 ALREADY_FOLLOWING` (= 既 accepted)
//!   など Misskey 慣行の `error.code` を返す
//!
//! ## scope
//!
//! Misskey 仕様で `following/create` / `following/delete` は **`write:following`**
//! scope を要求する。

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use sakurasato_core::repo;
use serde::Deserialize;
use serde_json::json;
use tracing::warn;

use crate::follow::{
    FollowError, FollowOutcome, FollowTarget, UnfollowOutcome, create_follow_core,
    delete_follow_core,
};
use crate::miauth::auth;
use crate::miauth::conv::from_actor_detailed;
use crate::miauth::error::error_resp;
use crate::state::AppState;

const SCOPE_WRITE_FOLLOWING: &str = "write:following";

#[derive(Debug, Deserialize, Default)]
pub struct FollowingBody {
    #[serde(default)]
    pub i: Option<String>,
    /// Misskey は userId を **string** で渡す。Sakurasato 内部 `i64` に parse する。
    #[serde(rename = "userId", default)]
    pub user_id: Option<String>,
}

/// `POST /api/following/create` handler。
pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<FollowingBody>>,
) -> Response {
    let body = body.map(|j| j.0).unwrap_or_default();
    let Some(_token_row) =
        auth::require_scope(&state, &headers, body.i.as_deref(), SCOPE_WRITE_FOLLOWING).await
    else {
        return auth::unauthorized("invalid or revoked token");
    };
    let Some(actor_id) = parse_user_id(body.user_id.as_deref()) else {
        return error_resp(StatusCode::NOT_FOUND, "NO_SUCH_USER", "no such user");
    };

    match create_follow_core(&state, FollowTarget::ActorId(actor_id)).await {
        Ok(outcome) => Json(build_create_response(&state, &outcome).await).into_response(),
        Err(err) => map_follow_error(&err, "create"),
    }
}

/// `POST /api/following/delete` handler。
pub async fn delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<FollowingBody>>,
) -> Response {
    let body = body.map(|j| j.0).unwrap_or_default();
    let Some(_token_row) =
        auth::require_scope(&state, &headers, body.i.as_deref(), SCOPE_WRITE_FOLLOWING).await
    else {
        return auth::unauthorized("invalid or revoked token");
    };
    let Some(target_id) = parse_user_id(body.user_id.as_deref()) else {
        return error_resp(StatusCode::NOT_FOUND, "NO_SUCH_USER", "no such user");
    };

    // Misskey wire は `userId` を取るが、Sakurasato の [`delete_follow_core`] は
    // `follow_id` を取る。userId → follow_id への解決を間に入れる ── viewer は
    // 常に local actor なので、`(follower=local, followed=userId)` で UNIQUE 行を
    // 引く。
    let Some(local) = resolve_local_actor_id(&state).await else {
        return error_resp(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "local actor initialization failed",
        );
    };
    let follow_row = match repo::follow::get_by_pair(state.pool(), local, target_id).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            return error_resp(
                StatusCode::NOT_FOUND,
                "NOT_FOLLOWING",
                "you are not following this user",
            );
        }
        Err(err) => {
            tracing::error!(
                ?err,
                target_id,
                "miauth following/delete: get_by_pair failed"
            );
            return error_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "follow lookup failed",
            );
        }
    };

    match delete_follow_core(&state, follow_row.id).await {
        Ok(outcome) => Json(build_delete_response(&state, &outcome).await).into_response(),
        Err(err) => map_follow_error(&err, "delete"),
    }
}

/// Misskey `following/create` の成功 body ── 相手 user の `UserDetailed` を返す。
/// `target` actor は [`FollowOutcome::target`] 由来。`MissUser` の follower
/// count は target actor 視点なので、`count_followers` を別途引く。
async fn build_create_response(state: &AppState, outcome: &FollowOutcome) -> serde_json::Value {
    let followers = repo::follow::count_followers(state.pool(), outcome.target.id)
        .await
        .unwrap_or(0);
    let following = repo::follow::count_following(state.pool(), outcome.target.id)
        .await
        .unwrap_or(0);
    let notes = if outcome.target.is_local {
        repo::note::count_local(state.pool()).await.unwrap_or(0)
    } else {
        0
    };
    from_actor_detailed(&outcome.target, followers, following, notes)
}

/// `following/delete` の成功 body ── unfollow した相手 user の `UserDetailed`。
async fn build_delete_response(state: &AppState, outcome: &UnfollowOutcome) -> serde_json::Value {
    // target_ap_id から actor row を再 fetch ── unfollow 後でも actor 行は残っている
    // (= 我々が知っている actor の master データなので follow 関係とは独立)。
    match repo::actor::get_by_ap_id(state.pool(), &outcome.target_ap_id).await {
        Ok(Some(actor)) => {
            let followers = repo::follow::count_followers(state.pool(), actor.id)
                .await
                .unwrap_or(0);
            let following = repo::follow::count_following(state.pool(), actor.id)
                .await
                .unwrap_or(0);
            let notes = if actor.is_local {
                repo::note::count_local(state.pool()).await.unwrap_or(0)
            } else {
                0
            };
            from_actor_detailed(&actor, followers, following, notes)
        }
        _ => {
            // actor 行が消えていても unfollow 自体は成功している。空 object で返す。
            json!({})
        }
    }
}

async fn resolve_local_actor_id(state: &AppState) -> Option<i64> {
    let host = &state.config().server.host;
    let user = &state.config().server.user;
    match repo::actor::get_by_username_host(state.pool(), user, host).await {
        Ok(Some(row)) if row.is_local => Some(row.id),
        _ => None,
    }
}

fn parse_user_id(s: Option<&str>) -> Option<i64> {
    s.and_then(|t| t.parse::<i64>().ok())
}

/// `FollowError` を Misskey 互換 error response にマップする。
fn map_follow_error(err: &FollowError, op: &str) -> Response {
    match err {
        FollowError::BadRequest(msg) => {
            warn!(error = %msg, op, "miauth following: bad request");
            error_resp(StatusCode::BAD_REQUEST, "INVALID_PARAM", msg)
        }
        FollowError::BadGateway(msg) => {
            warn!(error = %msg, op, "miauth following: upstream failure");
            error_resp(StatusCode::BAD_GATEWAY, "FEDERATION_ERROR", msg)
        }
        FollowError::NotFound(msg) => {
            warn!(error = %msg, op, "miauth following: not found");
            error_resp(StatusCode::NOT_FOUND, "NO_SUCH_USER", msg)
        }
        FollowError::Unavailable(msg) => {
            warn!(error = %msg, op, "miauth following: unavailable");
            error_resp(StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE", msg)
        }
        FollowError::Conflict(msg) => {
            warn!(error = %msg, op, "miauth following: conflict");
            error_resp(StatusCode::CONFLICT, "ALREADY_FOLLOWING", msg)
        }
        FollowError::Forbidden(msg) => {
            warn!(error = %msg, op, "miauth following: forbidden");
            error_resp(StatusCode::FORBIDDEN, "PERMISSION_DENIED", msg)
        }
        FollowError::Internal(e) => {
            tracing::error!(error = ?e, op, "miauth following: internal failure");
            error_resp(
                StatusCode::SERVICE_UNAVAILABLE,
                "INTERNAL_ERROR",
                "follow operation failed; check server logs",
            )
        }
    }
}
