//! M13 PR3 (Issue #79): `GET /api/v1/following` / `GET /api/v1/followers`。
//!
//! TUI の **`FollowList` 画面** が叩く読み出し API。`follow_request` 系統
//! (= pending 管理) と区別するため、ここは **`accepted` 行のみ** を返す。
//! pending を含めた一覧が欲しい場合は [`super::follow_request::list`] を使う。
//!
//! ## ルート
//!
//! - `GET /api/v1/following?limit=&before_id=` ── ローカル actor が
//!   `accepted` で follow している actor を `follow.id DESC` 順で返す。
//! - `GET /api/v1/followers?limit=&before_id=` ── ローカル actor を
//!   `accepted` で follow している actor を `follow.id DESC` 順で返す。
//!
//! どちらもページネーションは `follow.id` (= 自分が follow を張った順 /
//! 相手が我々をフォローした順) でカーソルを切る。`actor.id` ではなく
//! `follow.id` を使うことで、同じ actor を unfollow → 再 follow した時に
//! 最新フォロー順で再浮上する。
//!
//! ## エラー
//!
//! - 503: ローカル actor 未 init / DB アクセス失敗。
//! - 200 + 空配列: フォロー / フォロワーが居ない初期状態。

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use sakurasato_core::model::ActorRow;
use sakurasato_core::repo;
use sakurasato_core::repo::follow::FollowWithActor;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::error;

use crate::local_api::timeline as timeline_api;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub before_id: Option<i64>,
}

/// レスポンスの 1 行。`actor` は [`ActorRow`] をそのまま入れる ──
/// `#[serde(skip)]` で private 鍵フィールドは漏れない。`follow_id` は
/// `DELETE /api/v1/follow/{follow_id}` の引数として TUI 側がそのまま使う。
#[derive(Debug, Serialize)]
pub struct FollowListEntry {
    pub follow_id: i64,
    pub follow_state: String,
    pub follow_created_at: chrono::DateTime<chrono::Utc>,
    pub actor: ActorRow,
}

#[derive(Debug, Serialize)]
pub struct FollowListResponse {
    pub entries: Vec<FollowListEntry>,
    /// 次ページを取るときの `before_id` (= 最後の `follow.id`)。
    /// `entries` が空のときは `None`。
    pub next_before_id: Option<i64>,
}

/// `GET /api/v1/following` ── 自分が follow している accepted 一覧。
pub async fn following(State(state): State<AppState>, Query(q): Query<ListQuery>) -> Response {
    let local = match resolve_local_actor(&state).await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let limit = timeline_api::clamp_limit(q.limit);
    let rows = match repo::follow::list_following(state.pool(), local.id, q.before_id, limit).await
    {
        Ok(rows) => rows,
        Err(err) => {
            error!(?err, local_id = local.id, "list_following query failed");
            return service_unavailable("follow list query failed; check server logs");
        }
    };
    Json(to_response(rows)).into_response()
}

/// `GET /api/v1/followers` ── 自分を follow している accepted 一覧。
pub async fn followers(State(state): State<AppState>, Query(q): Query<ListQuery>) -> Response {
    let local = match resolve_local_actor(&state).await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let limit = timeline_api::clamp_limit(q.limit);
    let rows = match repo::follow::list_followers(state.pool(), local.id, q.before_id, limit).await
    {
        Ok(rows) => rows,
        Err(err) => {
            error!(?err, local_id = local.id, "list_followers query failed");
            return service_unavailable("follower list query failed; check server logs");
        }
    };
    Json(to_response(rows)).into_response()
}

fn to_response(rows: Vec<FollowWithActor>) -> FollowListResponse {
    let next_before_id = rows.last().map(|r| r.follow_id);
    let entries = rows
        .into_iter()
        .map(|r| FollowListEntry {
            follow_id: r.follow_id,
            follow_state: r.follow_state,
            follow_created_at: r.follow_created_at,
            actor: r.actor,
        })
        .collect();
    FollowListResponse {
        entries,
        next_before_id,
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
                "follow list operation failed; check server logs",
            ))
        }
    }
}

fn service_unavailable(msg: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "error": msg })),
    )
        .into_response()
}
