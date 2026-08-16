//! `POST /api/following/requests/{list,accept,reject,cancel}` (親 issue #150 続き)。
//!
//! Misskey 互換 ── 鍵アカ運用 (`actor.manually_approves_followers = TRUE`,
//! Issue #66 / M12) で `pending` のまま据え置かれた inbound Follow を、Aria 等
//! のクライアントから確認・承認・拒否するための経路。`cancel` はその逆
//! 方向 ── 自分 (me) が送信した outbound Follow の取り下げ (Misskey の
//! フォローボタン UI: requested 表示 → cancel ボタン) に対応する。
//!
//! ## 動機
//!
//! Aria の `FollowRequestsNotifier` (フォローリクエスト画面) は起動直後に
//! `following/requests/list` を呼ぶが、Sakurasato はこの endpoint 自体を
//! 実装していなかったため 404 → `ApiService.post` が例外を投げてクラッシュ
//! していた (`MisskeyFollowingRequests.list` → `FollowRequestsNotifier.build`)。
//! 承認 / 拒否操作 (`accept` / `reject`) も同じ画面から呼ばれるため合わせて
//! 実装する。`cancel` は Aria の `MisskeyFollowingRequests.cancel`
//! (`POST /api/following/requests/cancel`) が同様に 404 で crash していた
//! ギャップ (aria issue) への対応。
//!
//! 既存 CLI (`sakurasato-server follow-request list/approve/reject`) と同じ
//! core ロジック ([`crate::follow_request`]) を薄く HTTP に持ち上げるだけで、
//! 新しい承認フローは増やさない。`cancel` は M13 PR2 の
//! [`crate::follow::delete_follow_core`] (outbound Undo Follow + 行削除) を流用
//! する。
//!
//! ## wire 仕様 (clean-room, api-doc.misskey.io / misskey.io/api.json)
//!
//! - `following/requests/list`   : body `{ i }` → `[{ id, follower, followee }]`
//! - `following/requests/accept` : body `{ i, userId }` → `{}` (204 相当)
//! - `following/requests/reject` : body `{ i, userId }` → `{}`
//! - `following/requests/cancel` : body `{ i, userId }` → `{}` (Misskey は
//!   `UserLite` だが、aria は cancel の戻り値を使わない void 実装)
//!
//! `userId` は **follow request 自体の id ではなく相手 actor の userId**
//! (Misskey 仕様)。`(follower, followed)` に UNIQUE 制約があるため
//! `(follower=userId, followed=me)` で pending 行が高々 1 つに定まる。
//! `cancel` は引数の向きが**逆**で `(follower=me, followed=userId)` を引く
//! (= 自分が送った行)。
//!
//! ## scope
//!
//! `list` は `read:account`、`accept`/`reject`/`cancel` は
//! [`crate::miauth::following`] と同じ `write:following`。
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
use crate::miauth::conv::{from_actor_and_counts, resolve_user_emojis};
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
    let _token_row =
        match auth::require_scope(&state, &headers, body.i.as_deref(), SCOPE_READ_ACCOUNT).await {
            Ok(t) => t,
            Err(e) => return e.into_response(),
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

    // count は actor 種別で読み分ける (local = 実クエリ / remote = Collection
    // `totalItems` キャッシュ) ── [`crate::miauth::counts::counts_for_actor`]。
    let (me_followers, me_following, me_notes) =
        crate::miauth::counts::counts_for_actor(&state, &me).await;
    let host = state.config().server.host.clone();
    let me_emojis = resolve_user_emojis(state.pool(), &host, &me).await;
    let followee = from_actor_and_counts(&me, me_followers, me_following, me_notes, me_emojis);

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let follow_id = row.id;
        let follower_ap_id = row.follower_ap_id;
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
        let (f_followers, f_following, f_notes) =
            crate::miauth::counts::counts_for_actor(&state, &follower_actor).await;
        let follower_emojis = resolve_user_emojis(state.pool(), &host, &follower_actor).await;
        let follower = from_actor_and_counts(
            &follower_actor,
            f_followers,
            f_following,
            f_notes,
            follower_emojis,
        );
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

/// `POST /api/following/requests/cancel` handler ── 自分 (me) が**送信した**
/// pending Follow を取り下げる (Undo Follow を相手に配送 + follow 行削除)。
///
/// Misskey wire (`misskey.io/api.json`): body `{ i, userId }`, permission
/// `write:following`。`userId` は cancel の宛先 (= 自分がフォローしようとした
/// 相手)。accept / reject とは引数の向きが逆で `(follower=me, followed=userId)`
/// を引く。
///
/// [`crate::follow::delete_follow_core`] は state を問わず行を削除するため、
/// この層で `pending` 限定にガードする (= accepted のフォローが cancel で
/// 消える事故を防ぐ。Misskey の `following/requests/cancel` は pending の
/// 取り下げがセマンティクス)。
pub async fn cancel(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<MutateBody>>,
) -> Response {
    let body = body.map(|j| j.0).unwrap_or_default();
    let _token_row =
        match auth::require_scope(&state, &headers, body.i.as_deref(), SCOPE_WRITE_FOLLOWING).await
        {
            Ok(t) => t,
            Err(e) => return e.into_response(),
        };
    let Some(target_id) = body.user_id.as_deref().and_then(|s| s.parse::<i64>().ok()) else {
        return error_resp(StatusCode::NOT_FOUND, "NO_SUCH_USER", "no such user");
    };
    let Some(me) = resolve_self_actor(&state).await else {
        return error_resp(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "local actor initialization failed",
        );
    };

    // Misskey wire は `userId` (= cancel の宛先) を取るが、内部
    // `delete_follow_core` は `follow.id` を取る。`(follower=me, followed=userId)`
    // の UNIQUE 行を引いて変換する ── [`crate::miauth::following::delete`] と
    // 同じ間接パターン。
    let follow_row = match repo::follow::get_by_pair(state.pool(), me.id, target_id).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            return error_resp(
                StatusCode::BAD_REQUEST,
                "FOLLOW_REQUEST_NOT_FOUND",
                "no pending follow request to this user",
            );
        }
        Err(err) => {
            tracing::error!(
                ?err,
                target_id,
                "miauth following/requests/cancel: get_by_pair failed"
            );
            return error_resp(
                StatusCode::SERVICE_UNAVAILABLE,
                "INTERNAL_ERROR",
                "follow request lookup failed",
            );
        }
    };

    if follow_row.state != FollowState::Pending.as_str() {
        return error_resp(
            StatusCode::BAD_REQUEST,
            "FOLLOW_REQUEST_NOT_FOUND",
            "follow request already handled",
        );
    }

    match crate::follow::delete_follow_core(&state, follow_row.id).await {
        Ok(_outcome) => Json(json!({})).into_response(),
        Err(err) => {
            tracing::error!(
                ?err,
                follow_id = follow_row.id,
                target_id,
                "miauth following/requests/cancel: delete_follow_core failed"
            );
            error_resp(
                StatusCode::SERVICE_UNAVAILABLE,
                "INTERNAL_ERROR",
                "follow request cancellation failed; check server logs",
            )
        }
    }
}

async fn mutate(
    state: AppState,
    headers: HeaderMap,
    body: Option<Json<MutateBody>>,
    new_state: FollowState,
) -> Response {
    let body = body.map(|j| j.0).unwrap_or_default();
    let _token_row =
        match auth::require_scope(&state, &headers, body.i.as_deref(), SCOPE_WRITE_FOLLOWING).await
        {
            Ok(t) => t,
            Err(e) => return e.into_response(),
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
