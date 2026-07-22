//! リスト機能 ── フォロー中ユーザーをグルーピングした専用タイムライン。
//!
//! Mastodon/Misskey 互換の「リスト」を TUI ローカル API 経由で操作する。
//! `MiAuth` 側 (`users/lists/*` + `notes/user-list-timeline`, Aria 等の Misskey
//! クライアント向け) は [`crate::miauth::lists`] に別実装があり、両者とも
//! [`sakurasato_core::repo::user_list`] を共有する。
//!
//! ## ルート
//!
//! - `GET    /api/v1/lists`                     ── 一覧 (`member_count` 込み)
//! - `POST   /api/v1/lists`                      ── 作成 `{ "title": "..." }`
//! - `GET    /api/v1/lists/{id}`                  ── 詳細 (メンバー `ActorRow` 一覧込み)
//! - `PATCH  /api/v1/lists/{id}`                  ── リネーム `{ "title": "..." }`
//! - `DELETE /api/v1/lists/{id}`                  ── 削除
//! - `POST   /api/v1/lists/{id}/members`          ── メンバー追加 `{ "actor_id": N }`
//! - `DELETE /api/v1/lists/{id}/members/{actor_id}` ── メンバー削除
//! - `GET    /api/v1/timeline/list/{id}?limit=&before_ts_ms=` ── リストタイムライン
//!
//! メンバー追加は「`follow.state = 'accepted'` の相手のみ」 ──
//! [`sakurasato_core::repo::user_list::add_member`] 参照。

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, TimeZone, Utc};
use sakurasato_core::model::{ActorRow, UserListRow};
use sakurasato_core::repo;
use sakurasato_core::repo::user_list::{AddMemberError, UserListWithCount};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::error;

use crate::local_api::timeline::{TimelineQuery, clamp_limit, merge_and_respond};
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct CreateListRequest {
    pub title: String,
}

#[derive(Debug, Deserialize)]
pub struct RenameListRequest {
    pub title: String,
}

#[derive(Debug, Deserialize)]
pub struct AddMemberRequest {
    pub actor_id: i64,
}

#[derive(Debug, Serialize)]
pub struct UserListDto {
    pub id: i64,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub member_count: i64,
}

impl From<UserListWithCount> for UserListDto {
    fn from(v: UserListWithCount) -> Self {
        Self {
            id: v.list.id,
            title: v.list.title,
            created_at: v.list.created_at,
            updated_at: v.list.updated_at,
            member_count: v.member_count,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct UserListDetailDto {
    pub id: i64,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// `#[serde(skip)]` された `ActorRow` の秘密鍵フィールドは漏れない
    /// (`follow_list::FollowListEntry` と同じ流儀)。
    pub members: Vec<ActorRow>,
}

/// `GET /api/v1/lists` ── 一覧。
pub async fn list(State(state): State<AppState>) -> Response {
    match repo::user_list::list_all_with_counts(state.pool()).await {
        Ok(rows) => {
            let items: Vec<UserListDto> = rows.into_iter().map(UserListDto::from).collect();
            Json(json!({ "items": items })).into_response()
        }
        Err(err) => {
            error!(?err, "GET /api/v1/lists: query failed");
            service_unavailable("list query failed; check server logs")
        }
    }
}

/// `POST /api/v1/lists` ── 作成。
pub async fn create(State(state): State<AppState>, Json(req): Json<CreateListRequest>) -> Response {
    let title = req.title.trim();
    if title.is_empty() {
        return bad_request("title must not be empty");
    }
    match repo::user_list::create(state.pool(), title).await {
        Ok(row) => (StatusCode::CREATED, Json(to_dto(row, 0))).into_response(),
        Err(err) => {
            error!(?err, "POST /api/v1/lists: insert failed");
            service_unavailable("list creation failed; check server logs")
        }
    }
}

/// `GET /api/v1/lists/{id}` ── 詳細 (メンバー込み)。
pub async fn show(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let Some(row) = (match repo::user_list::get_by_id(state.pool(), id).await {
        Ok(v) => v,
        Err(err) => {
            error!(?err, id, "GET /api/v1/lists/{id}: lookup failed");
            return service_unavailable("list lookup failed; check server logs");
        }
    }) else {
        return not_found(id);
    };
    let member_ids = match repo::user_list::list_member_ids(state.pool(), id).await {
        Ok(v) => v,
        Err(err) => {
            error!(?err, id, "GET /api/v1/lists/{id}: member id lookup failed");
            return service_unavailable("list lookup failed; check server logs");
        }
    };
    let members = match repo::actor::list_by_ids(state.pool(), &member_ids).await {
        Ok(v) => v,
        Err(err) => {
            error!(
                ?err,
                id, "GET /api/v1/lists/{id}: member actor lookup failed"
            );
            return service_unavailable("list lookup failed; check server logs");
        }
    };
    Json(UserListDetailDto {
        id: row.id,
        title: row.title,
        created_at: row.created_at,
        updated_at: row.updated_at,
        members,
    })
    .into_response()
}

/// `PATCH /api/v1/lists/{id}` ── リネーム。
pub async fn rename(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<RenameListRequest>,
) -> Response {
    let title = req.title.trim();
    if title.is_empty() {
        return bad_request("title must not be empty");
    }
    match repo::user_list::rename(state.pool(), id, title).await {
        Ok(Some(row)) => {
            let member_count = repo::user_list::count_members(state.pool(), row.id)
                .await
                .unwrap_or(0);
            Json(to_dto(row, member_count)).into_response()
        }
        Ok(None) => not_found(id),
        Err(err) => {
            error!(?err, id, "PATCH /api/v1/lists/{id}: update failed");
            service_unavailable("list rename failed; check server logs")
        }
    }
}

/// `DELETE /api/v1/lists/{id}` ── 削除。
pub async fn delete(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    match repo::user_list::delete_by_id(state.pool(), id).await {
        Ok(0) => not_found(id),
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => {
            error!(?err, id, "DELETE /api/v1/lists/{id}: delete failed");
            service_unavailable("list delete failed; check server logs")
        }
    }
}

/// `POST /api/v1/lists/{id}/members` ── メンバー追加。
pub async fn add_member(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<AddMemberRequest>,
) -> Response {
    let local = match resolve_local_actor(&state).await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    match repo::user_list::add_member(state.pool(), id, local.id, req.actor_id).await {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(AddMemberError::ListNotFound)) => not_found(id),
        Ok(Err(AddMemberError::NotFollowing)) => bad_request(
            "actor_id is not followed (state=accepted) by the local actor; follow first",
        ),
        Err(err) => {
            error!(
                ?err,
                id,
                actor_id = req.actor_id,
                "POST /api/v1/lists/{id}/members: failed"
            );
            service_unavailable("list member add failed; check server logs")
        }
    }
}

/// `DELETE /api/v1/lists/{id}/members/{actor_id}` ── メンバー削除。
pub async fn remove_member(
    State(state): State<AppState>,
    Path((id, actor_id)): Path<(i64, i64)>,
) -> Response {
    match repo::user_list::remove_member(state.pool(), id, actor_id).await {
        Ok(0) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("actor_id={actor_id} is not a member of list {id}") })),
        )
            .into_response(),
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => {
            error!(
                ?err,
                id, actor_id, "DELETE /api/v1/lists/{id}/members/{actor_id}: failed"
            );
            service_unavailable("list member remove failed; check server logs")
        }
    }
}

/// `GET /api/v1/timeline/list/{id}` ── リストタイムライン。
pub async fn timeline(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<TimelineQuery>,
) -> Response {
    if repo::user_list::get_by_id(state.pool(), id)
        .await
        .unwrap_or(None)
        .is_none()
    {
        return not_found(id);
    }
    let local = match resolve_local_actor(&state).await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let host = &state.config().server.host;
    let limit = clamp_limit(q.limit);
    let until_ts: Option<DateTime<Utc>> = q
        .before_ts_ms
        .and_then(|ms| Utc.timestamp_millis_opt(ms).single());

    let note_entries = match repo::user_list::list_list_timeline_window(
        state.pool(),
        id,
        None,
        None,
        None,
        until_ts,
        limit,
    )
    .await
    {
        Ok(rows) => rows,
        Err(err) => {
            error!(?err, id, "timeline/list: list_list_timeline_window failed");
            return service_unavailable("list timeline query failed; check server logs");
        }
    };
    let renote_rows = match repo::announce::list_list_renote_window(
        state.pool(),
        id,
        local.id,
        None,
        until_ts,
        limit,
    )
    .await
    {
        Ok(rows) => rows,
        Err(err) => {
            error!(?err, id, "timeline/list: list_list_renote_window failed");
            return service_unavailable("list timeline query failed; check server logs");
        }
    };

    merge_and_respond(
        state.pool(),
        host,
        local.id,
        note_entries,
        renote_rows,
        limit,
        "timeline/list",
    )
    .await
}

fn to_dto(row: UserListRow, member_count: i64) -> UserListDto {
    UserListDto {
        id: row.id,
        title: row.title,
        created_at: row.created_at,
        updated_at: row.updated_at,
        member_count,
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
                "list operation failed; check server logs",
            ))
        }
    }
}

fn not_found(id: i64) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": format!("no list with id={id}") })),
    )
        .into_response()
}

fn bad_request(msg: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))).into_response()
}

fn service_unavailable(msg: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "error": msg })),
    )
        .into_response()
}
