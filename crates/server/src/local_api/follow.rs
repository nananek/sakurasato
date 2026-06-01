//! M13 PR2 (Issue #79): `POST /api/v1/follow` / `DELETE /api/v1/follow/{id}`。
//!
//! TUI Profile 画面の `f` トグル (= follow / unfollow) と `:follow @acct` コマンド
//! が叩く経路。CLI (`sakurasato-server follow`) と同じ core ロジック
//! ([`crate::follow::create_follow_core`] / [`crate::follow::delete_follow_core`])
//! を共有する ── HTTP / CLI で挙動が二重メンテにならないよう。
//!
//! ## ルート
//!
//! - `POST /api/v1/follow` ── body は `{"acct": "..."}` / `{"actor_uri": "..."}`
//!   / `{"actor_id": 123}` のいずれか 1 つ。冪等 (`upsert_pending` 経路) で、
//!   既存 `accepted` 行があれば再送せず 200 を返す。
//! - `DELETE /api/v1/follow/{follow_id}` ── 本人 follow のみ削除可能 (403 ガード)。
//!   削除と同時に Undo Follow を `delivery_queue` に投入する。state は問わない
//!   (`pending` / `accepted` / `rejected` のいずれでも削除可能)。

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use crate::follow::{
    FollowError, FollowOutcome, FollowTarget, UnfollowOutcome, create_follow_core,
    delete_follow_core,
};
use crate::state::AppState;

/// `POST /api/v1/follow` の入力 body。
///
/// 3 つの target 指定方式は **排他**。複数指定した場合は優先順位で 1 つだけ
/// 採用するのではなく、明示的に 400 で弾く ── 「`actor_id` と acct を両方
/// 指定したが食い違っている」状況で意図が曖昧になるのを避ける。
#[derive(Debug, Deserialize)]
pub struct CreateFollowRequest {
    #[serde(default)]
    pub acct: Option<String>,
    #[serde(default)]
    pub actor_uri: Option<String>,
    #[serde(default)]
    pub actor_id: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct CreateFollowResponse {
    pub follow_id: i64,
    pub ap_id: String,
    pub state: String,
    pub target_actor_id: i64,
    pub target_ap_id: String,
    /// 既存 `accepted` 行を再叩きしたとき (= Follow を再送しなかった) は `None`、
    /// それ以外 (= pending 新規 / pending 再 enqueue / rejected → pending 復活)
    /// は `Some(delivery_queue.id)`。
    pub delivery_queue_id: Option<i64>,
    pub inbox_url: Option<String>,
    /// `delivery_queue_id is None` と同値だが、クライアント側で読みやすい
    /// boolean を返しておく (= TUI `f` トグルが UI 文言を分岐する用)。
    pub already_accepted: bool,
}

#[derive(Debug, Serialize)]
pub struct DeleteFollowResponse {
    pub follow_id: i64,
    pub target_ap_id: String,
    pub delivery_queue_id: i64,
    pub inbox_url: String,
}

pub async fn create(
    State(state): State<AppState>,
    Json(req): Json<CreateFollowRequest>,
) -> Response {
    let target = match parse_target(&req) {
        Ok(t) => t,
        Err(msg) => return bad_request(msg),
    };
    match create_follow_core(&state, target).await {
        Ok(outcome) => {
            info!(
                follow_id = outcome.follow.id,
                target = %outcome.target.ap_id,
                already_accepted = outcome.already_accepted,
                "POST /api/v1/follow ok"
            );
            (StatusCode::OK, Json(to_create_response(outcome))).into_response()
        }
        Err(err) => map_follow_error(&err, "create"),
    }
}

pub async fn delete(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    match delete_follow_core(&state, id).await {
        Ok(outcome) => {
            info!(
                follow_id = outcome.follow_id,
                target = %outcome.target_ap_id,
                queue_id = outcome.queue_id,
                "DELETE /api/v1/follow ok"
            );
            (StatusCode::OK, Json(to_delete_response(outcome))).into_response()
        }
        Err(err) => map_follow_error(&err, "delete"),
    }
}

fn parse_target(req: &CreateFollowRequest) -> Result<FollowTarget, String> {
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
                // unreachable: provided==1 implies one branch matched.
                Err("internal: target parser missed a case".into())
            }
        }
        _ => Err("provide only one of `acct` / `actor_uri` / `actor_id`".into()),
    }
}

fn to_create_response(outcome: FollowOutcome) -> CreateFollowResponse {
    CreateFollowResponse {
        follow_id: outcome.follow.id,
        ap_id: outcome.follow.ap_id.clone(),
        state: outcome.follow.state.clone(),
        target_actor_id: outcome.target.id,
        target_ap_id: outcome.target.ap_id.clone(),
        delivery_queue_id: outcome.queue_id,
        inbox_url: outcome.inbox_url,
        already_accepted: outcome.already_accepted,
    }
}

fn to_delete_response(outcome: UnfollowOutcome) -> DeleteFollowResponse {
    DeleteFollowResponse {
        follow_id: outcome.follow_id,
        target_ap_id: outcome.target_ap_id,
        delivery_queue_id: outcome.queue_id,
        inbox_url: outcome.inbox_url,
    }
}

/// `FollowError` を HTTP status + JSON body にマップする。
///
/// `Internal` だけはサーバログに詳細を残し、レスポンスは固定文言で 503 を返す
/// (= sqlx エラー本文や URL parse 詳細が client に漏れないように
/// `follow_request::mutate` と同じ作法)。
fn map_follow_error(err: &FollowError, op: &str) -> Response {
    match err {
        FollowError::BadRequest(msg) => {
            warn!(error = %msg, op, "/api/v1/follow: bad request");
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": msg })),
            )
                .into_response()
        }
        FollowError::BadGateway(msg) => {
            warn!(error = %msg, op, "/api/v1/follow: upstream failure");
            (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({ "error": msg })),
            )
                .into_response()
        }
        FollowError::NotFound(msg) => {
            warn!(error = %msg, op, "/api/v1/follow: not found");
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": msg })),
            )
                .into_response()
        }
        FollowError::Unavailable(msg) => {
            warn!(error = %msg, op, "/api/v1/follow: unavailable");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "error": msg })),
            )
                .into_response()
        }
        FollowError::Conflict(msg) => {
            warn!(error = %msg, op, "/api/v1/follow: conflict");
            (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": msg })),
            )
                .into_response()
        }
        FollowError::Forbidden(msg) => {
            warn!(error = %msg, op, "/api/v1/follow: forbidden");
            (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({ "error": msg })),
            )
                .into_response()
        }
        FollowError::Internal(e) => {
            error!(error = ?e, op, "/api/v1/follow: internal failure");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": "follow operation failed; check server logs"
                })),
            )
                .into_response()
        }
    }
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
            parse_target(&CreateFollowRequest {
                acct: None,
                actor_uri: None,
                actor_id: None,
            })
            .is_err()
        );
        assert!(
            parse_target(&CreateFollowRequest {
                acct: Some("alice@example.test".into()),
                actor_uri: Some("https://example.test/users/alice".into()),
                actor_id: None,
            })
            .is_err()
        );
        assert!(
            parse_target(&CreateFollowRequest {
                acct: Some("alice@example.test".into()),
                actor_uri: None,
                actor_id: Some(7),
            })
            .is_err()
        );
    }

    #[test]
    fn parse_target_accepts_acct() {
        let t = parse_target(&CreateFollowRequest {
            acct: Some("alice@example.test".into()),
            actor_uri: None,
            actor_id: None,
        })
        .unwrap();
        assert!(matches!(t, FollowTarget::Acct(s) if s == "alice@example.test"));
    }

    #[test]
    fn parse_target_accepts_actor_id() {
        let t = parse_target(&CreateFollowRequest {
            acct: None,
            actor_uri: None,
            actor_id: Some(42),
        })
        .unwrap();
        assert!(matches!(t, FollowTarget::ActorId(42)));
    }

    #[test]
    fn parse_target_accepts_actor_uri() {
        let t = parse_target(&CreateFollowRequest {
            acct: None,
            actor_uri: Some("https://example.test/users/alice".into()),
            actor_id: None,
        })
        .unwrap();
        assert!(matches!(t, FollowTarget::ActorUri(s) if s == "https://example.test/users/alice"));
    }
}
