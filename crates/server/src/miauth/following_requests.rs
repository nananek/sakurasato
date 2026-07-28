//! `POST /api/following/requests/{list,accept,reject}` (親 issue #150 続き)。
//!
//! Misskey 互換 ── 鍵アカ運用 (`actor.manually_approves_followers = TRUE`,
//! Issue #66 / M12) で `pending` のまま据え置かれた inbound Follow を、Aria 等
//! のクライアントから確認・承認・拒否するための経路。
//!
//! ## 動機
//!
//! Aria の `FollowRequestsNotifier` (フォローリクエスト画面) は起動直後に
//! `following/requests/list` を呼ぶが、Sakurasato はこの endpoint 自体を
//! 実装していなかったため 404 → `ApiService.post` が例外を投げてクラッシュ
//! していた (`MisskeyFollowingRequests.list` → `FollowRequestsNotifier.build`)。
//! 承認 / 拒否操作 (`accept` / `reject`) も同じ画面から呼ばれるため合わせて
//! 実装する。
//!
//! 既存 CLI (`sakurasato-server follow-request list/approve/reject`) と同じ
//! core ロジック ([`crate::follow_request`]) を薄く HTTP に持ち上げるだけで、
//! 新しい承認フローは増やさない。
//!
//! ## wire 仕様 (clean-room, api-doc.misskey.io)
//!
//! - `following/requests/list`   : body `{ i }` → `[{ id, follower, followee }]`
//! - `following/requests/accept` : body `{ i, userId }` → `{}` (204 相当)
//! - `following/requests/reject` : body `{ i, userId }` → `{}`
//!
//! `userId` は **follow request 自体の id ではなく follower の userId**
//! (Misskey 仕様)。`(follower, followed)` に UNIQUE 制約があるため
//! `(follower=userId, followed=me)` で pending 行が高々 1 つに定まる。
//!
//! ## scope
//!
//! `list` は `read:account`、`accept`/`reject` は [`crate::miauth::following`]
//! と同じ `write:following`。
//!
//! ## AGPL discipline
//!
//! [`crate::miauth`] module doc の `[[agpl-discipline-miauth]]` に従い、
//! Misskey の TypeScript handler は読まずに公開 API 仕様のみを一次資料とする。

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use sakurasato_core::model::FollowState;
use sakurasato_core::repo;
use serde::Deserialize;
use serde_json::json;
use tracing::warn;

use crate::follow_request::{MutateError, approve_or_reject};
use crate::miauth::auth;
use crate::miauth::conv::from_actor_and_counts;
use crate::miauth::error::error_resp;
use crate::miauth::notes::resolve_self_actor;
use crate::state::AppState;

const SCOPE_READ_ACCOUNT: &str = "read:account";
const SCOPE_WRITE_FOLLOWING: &str = "write:following";

#[derive(Debug, Deserialize, Default)]
pub struct ListBody {
    #[serde(default)]
    pub i: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct MutateBody {
    #[serde(default)]
    pub i: Option<String>,
    /// Misskey は userId を **string** で渡す ([`crate::miauth::following`] と同じ)。
    #[serde(rename = "userId", default)]
    pub user_id: Option<String>,
}

/// `POST /api/following/requests/list` handler ── 自分宛の `pending` Follow を
/// 受信順 (古い順) に列挙する。
// `follower` / `followee` は Misskey wire の AP 用語そのもの。alias でリネーム
// すると逆に読みづらいので、`follow` 系モジュール ([`crate::follow_request`] と
// 同じ理由) で許可する。
#[allow(clippy::similar_names)]
pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<ListBody>>,
) -> Response {
    let body = body.map(|j| j.0).unwrap_or_default();
    let Some(_token_row) =
        auth::require_scope(&state, &headers, body.i.as_deref(), SCOPE_READ_ACCOUNT).await
    else {
        return auth::unauthorized("invalid or revoked token");
    };
    let Some(me) = resolve_self_actor(&state).await else {
        return error_resp(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "local actor initialization failed",
        );
    };

    let rows = match repo::follow::list_for_local(state.pool(), Some("pending")).await {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(?err, "miauth following/requests/list: query failed");
            return error_resp(
                StatusCode::SERVICE_UNAVAILABLE,
                "INTERNAL_ERROR",
                "follow request lookup failed",
            );
        }
    };

    let me_followers = repo::follow::count_followers(state.pool(), me.id)
        .await
        .unwrap_or(0);
    let me_following = repo::follow::count_following(state.pool(), me.id)
        .await
        .unwrap_or(0);
    let me_notes = repo::note::count_local(state.pool()).await.unwrap_or(0);
    let followee = from_actor_and_counts(&me, me_followers, me_following, me_notes);

    let mut out = Vec::with_capacity(rows.len());
    for (follow_id, _ap_id, follower_ap_id, _state_str, _created_at) in rows {
        let follower_actor = match repo::actor::get_by_ap_id(state.pool(), &follower_ap_id).await {
            Ok(Some(actor)) => actor,
            Ok(None) => {
                // follow 行が指す actor が消えている (通常は起きない不整合)。
                // この 1 件だけ落として続行する ── 他の pending request の
                // 表示をブロックしない。
                warn!(
                    follow_id,
                    follower_ap_id, "miauth following/requests/list: dangling follower actor"
                );
                continue;
            }
            Err(err) => {
                tracing::error!(
                    ?err,
                    follow_id,
                    "miauth following/requests/list: actor lookup failed"
                );
                return error_resp(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "INTERNAL_ERROR",
                    "follow request lookup failed",
                );
            }
        };
        let f_followers = repo::follow::count_followers(state.pool(), follower_actor.id)
            .await
            .unwrap_or(0);
        let f_following = repo::follow::count_following(state.pool(), follower_actor.id)
            .await
            .unwrap_or(0);
        let f_notes = if follower_actor.is_local {
            repo::note::count_local(state.pool()).await.unwrap_or(0)
        } else {
            0
        };
        let follower = from_actor_and_counts(&follower_actor, f_followers, f_following, f_notes);
        out.push(json!({
            "id": follow_id.to_string(),
            "follower": follower,
            "followee": followee,
        }));
    }
    Json(out).into_response()
}

/// `POST /api/following/requests/accept` handler。
pub async fn accept(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<MutateBody>>,
) -> Response {
    mutate(state, headers, body, FollowState::Accepted).await
}

/// `POST /api/following/requests/reject` handler。
pub async fn reject(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<MutateBody>>,
) -> Response {
    mutate(state, headers, body, FollowState::Rejected).await
}

async fn mutate(
    state: AppState,
    headers: HeaderMap,
    body: Option<Json<MutateBody>>,
    new_state: FollowState,
) -> Response {
    let body = body.map(|j| j.0).unwrap_or_default();
    let Some(_token_row) =
        auth::require_scope(&state, &headers, body.i.as_deref(), SCOPE_WRITE_FOLLOWING).await
    else {
        return auth::unauthorized("invalid or revoked token");
    };
    let Some(follower_id) = body.user_id.as_deref().and_then(|s| s.parse::<i64>().ok()) else {
        return error_resp(StatusCode::NOT_FOUND, "NO_SUCH_USER", "no such user");
    };
    let Some(me) = resolve_self_actor(&state).await else {
        return error_resp(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "local actor initialization failed",
        );
    };

    // Misskey wire は `userId` (= follower 側) を取るが、内部 `approve_or_reject`
    // は `follow.id` を取る。`(follower, followed=me)` の UNIQUE 行を引いて
    // 変換する ── [`crate::miauth::following::delete`] と同じ間接パターン。
    let follow_row = match repo::follow::get_by_pair(state.pool(), follower_id, me.id).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            return error_resp(
                StatusCode::NOT_FOUND,
                "FOLLOW_REQUEST_NOT_FOUND",
                "no pending follow request from this user",
            );
        }
        Err(err) => {
            tracing::error!(
                ?err,
                follower_id,
                "miauth following/requests mutate: get_by_pair failed"
            );
            return error_resp(
                StatusCode::SERVICE_UNAVAILABLE,
                "INTERNAL_ERROR",
                "follow request lookup failed",
            );
        }
    };

    match approve_or_reject(&state, follow_row.id, new_state).await {
        Ok(()) => Json(json!({})).into_response(),
        Err(MutateError::NotFound(_)) => error_resp(
            StatusCode::NOT_FOUND,
            "FOLLOW_REQUEST_NOT_FOUND",
            "no such follow request",
        ),
        Err(err @ MutateError::NotPending { .. }) => {
            warn!(error = %err, follower_id, ?new_state, "miauth following/requests mutate: not pending");
            error_resp(
                StatusCode::BAD_REQUEST,
                "FOLLOW_REQUEST_NOT_FOUND",
                "follow request already handled",
            )
        }
        Err(err @ MutateError::NotForLocal { .. }) => {
            warn!(error = %err, follower_id, "miauth following/requests mutate: not for local");
            error_resp(
                StatusCode::BAD_REQUEST,
                "INVALID_PARAM",
                "follow request not addressed to local actor",
            )
        }
        Err(MutateError::Infra(err)) => {
            tracing::error!(
                ?err,
                follower_id,
                "miauth following/requests mutate: infra failure"
            );
            error_resp(
                StatusCode::SERVICE_UNAVAILABLE,
                "INTERNAL_ERROR",
                "follow request mutation failed; check server logs",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    // DB を要する経路 (require_scope / repo lookup) は
    // `crates/server/tests/miauth_write_pg.rs` の sqlx::test 統合テストで
    // カバーする (= following.rs / lists.rs と同じ流儀)。ここでは pure な
    // body parsing だけ確認する。
    use super::*;

    #[test]
    fn mutate_body_parses_string_user_id() {
        let body: MutateBody = serde_json::from_value(json!({"i": "tok", "userId": "42"})).unwrap();
        assert_eq!(body.user_id.as_deref(), Some("42"));
    }
}
