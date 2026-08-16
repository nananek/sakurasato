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
//!   オブジェクトを返す)。[`crate::miauth::conv::from_actor_detailed`] に
//!   [`crate::follow::compute_follow_relationship`] の結果を渡し、`isFollowing` /
//!   `isFollowed` / `hasPendingFollowRequestFromYou` / `hasPendingFollowRequestToYou`
//!   を載せる。当初は「別 endpoint `users/relation` で実装予定」としていたが、
//!   misskey-hub.net の公式 API 仕様にそのような endpoint は存在せず、
//!   `users/show` / `following/create` 等のレスポンス自体にこれらのフィールドを
//!   含めるのが正しい仕様だった (PR #348 系で修正)。
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
use crate::miauth::conv::{from_actor_detailed, resolve_user_emojis};
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
    let _token_row =
        match auth::require_scope(&state, &headers, body.i.as_deref(), SCOPE_WRITE_FOLLOWING).await
        {
            Ok(t) => t,
            Err(e) => return e.into_response(),
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
    let _token_row =
        match auth::require_scope(&state, &headers, body.i.as_deref(), SCOPE_WRITE_FOLLOWING).await
        {
            Ok(t) => t,
            Err(e) => return e.into_response(),
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
/// count は target actor 視点なので、[`crate::miauth::counts::counts_for_actor`]
/// で actor 種別に読み分けて引く (remote は Collection `totalItems` キャッシュ。
/// 表示前に `users/show` のプロフィール経路
/// [`crate::remote_actor::refresh_remote_actor_if_stale`] が取得・更新する ──
/// Aria はプロフィール画面 → フォローの順で叩くため、実用上は直前に新鮮化
/// されている)。
///
/// relationship (= `isFollowing` / `isFollowed` / `hasPendingFollowRequest*`)
/// は follow 直後の実状態を [`crate::follow::compute_follow_relationship`] で
/// 引いて載せる。local actor id が解決できない (init 未実行) 場合は
/// `followers`/`following` count と同じく中立値でフェイルオープンする。
async fn build_create_response(state: &AppState, outcome: &FollowOutcome) -> serde_json::Value {
    let (followers, following, notes) =
        crate::miauth::counts::counts_for_actor(state, &outcome.target).await;
    let rel = compute_relationship_or_neutral(state, outcome.target.id).await;
    let emojis =
        resolve_user_emojis(state.pool(), &state.config().server.host, &outcome.target).await;
    from_actor_detailed(&outcome.target, followers, following, notes, rel, emojis)
}

/// `following/delete` の成功 body ── unfollow した相手 user の `UserDetailed`。
async fn build_delete_response(state: &AppState, outcome: &UnfollowOutcome) -> serde_json::Value {
    // target_ap_id から actor row を再 fetch ── unfollow 後でも actor 行は残っている
    // (= 我々が知っている actor の master データなので follow 関係とは独立)。
    match repo::actor::get_by_ap_id(state.pool(), &outcome.target_ap_id).await {
        Ok(Some(actor)) => {
            let (followers, following, notes) =
                crate::miauth::counts::counts_for_actor(state, &actor).await;
            let rel = compute_relationship_or_neutral(state, actor.id).await;
            let emojis =
                resolve_user_emojis(state.pool(), &state.config().server.host, &actor).await;
            from_actor_detailed(&actor, followers, following, notes, rel, emojis)
        }
        _ => {
            // actor 行が消えていても unfollow 自体は成功している。空 object で返す。
            json!({})
        }
    }
}

/// viewer (= ローカル actor) から見た `target_actor_id` との follow relationship
/// を計算する。local actor 未 init / DB 障害時は中立値にフェイルオープンする
/// (= `followers`/`following` count の `.unwrap_or(0)` と同じ方針)。
async fn compute_relationship_or_neutral(
    state: &AppState,
    target_actor_id: i64,
) -> crate::follow::FollowRelationship {
    match resolve_local_actor_id(state).await {
        Some(local_id) => crate::follow::compute_follow_relationship(
            state.pool(),
            local_id,
            target_actor_id,
        )
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(
                ?err,
                target_actor_id,
                "miauth following: relationship computation failed; falling back to neutral",
            );
            crate::follow::FollowRelationship::neutral()
        }),
        None => crate::follow::FollowRelationship::neutral(),
    }
}

/// ローカル actor の DB id を引く。未 init / 非 local のときは `None`。
pub(super) async fn resolve_local_actor_id(state: &AppState) -> Option<i64> {
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
