//! `POST /api/users/lists/*` + `POST /api/notes/user-list-timeline`
//! ── Mastodon/Misskey 互換の「リスト」機能 (Aria 等の Misskey クライアント向け)。
//!
//! TUI ローカル API 側 ([`crate::local_api::user_list`]) と同じ
//! [`sakurasato_core::repo::user_list`] を共有する。
//!
//! ## wire 仕様 (clean-room)
//!
//! - <https://api-doc.misskey.io/> (`users/lists/*`, `notes/user-list-timeline`)
//! - <https://misskey-hub.net/>
//!
//! [`crate::miauth`] module doc の AGPL discipline に従い、Misskey の
//! TypeScript handler は読まずに observed wire shape のみを一次資料とする。
//!
//! ## scope
//!
//! Misskey 本家に `read:lists` / `write:lists` という専用 scope は存在せず、
//! `users/lists/*` は `read:account`(参照系) / `write:account`(変更系) を
//! 要求する。本実装もそれに合わせる ── ただし本 repo の parity test
//! (`tests/federation/test_miauth_read_parity.py` / `test_miauth_write_parity.py`)
//! で実 Misskey に対して実証確認すること。
//!
//! ## Sakurasato 固有の制約 (Misskey 本家との差異)
//!
//! Misskey 本家はフォロー関係の無い相手でもリストに追加できるが、Sakurasato
//! では **`follow.state = 'accepted'` の相手 (+ 自分自身) のみ** 追加を許可
//! する (`sakurasato_core::repo::user_list::add_member` 参照)。お一人様サーバで
//! 「フォローすらしていない相手をリスト管理する」実用上の需要が薄い一方、
//! 既知の accepted actor に限定することで actor 解決 (`WebFinger` 等) を経ずに
//! 常にローカル DB の行だけで完結させられる。**自分自身は例外的に無条件で
//! 追加できる** (= 「自分の投稿も混ぜたリスト」を作れるようにするため。
//! 自分自身を follow する概念が無いので accepted-follow チェックの対象外)。
//! 違反時は `NOT_FOLLOWING` エラー (= [`crate::miauth::following`] の同名
//! コードと同じ意味) を返す。

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use sakurasato_core::repo;
use sakurasato_core::repo::user_list::AddMemberError;
use serde::Deserialize;

use crate::miauth::auth;
use crate::miauth::conv::{
    EMPTY_EMOJIS, MissNote, NoteSummary, build_renote_miss_note, bulk_load_note_summaries,
    from_actor_and_counts, resolve_user_emojis_by_ids, timeline_entry_to_miss_note,
    user_list_to_miss,
};
use crate::miauth::error::{bad_request, error_resp};
use crate::miauth::notes::{ms_epoch_to_datetime, resolve_cursor_ts, resolve_self_actor_id};
use crate::state::AppState;

const SCOPE_READ_ACCOUNT: &str = "read:account";
const SCOPE_WRITE_ACCOUNT: &str = "write:account";

const TIMELINE_LIMIT_DEFAULT: i64 = 10;
const TIMELINE_LIMIT_MAX: i64 = 100;

#[derive(Debug, Deserialize, Default)]
pub struct CreateBody {
    #[serde(default)]
    pub i: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ListOnlyBody {
    #[serde(default)]
    pub i: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ShowBody {
    #[serde(default)]
    pub i: Option<String>,
    #[serde(rename = "listId", default)]
    pub list_id: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct UpdateBody {
    #[serde(default)]
    pub i: Option<String>,
    #[serde(rename = "listId", default)]
    pub list_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct MemberBody {
    #[serde(default)]
    pub i: Option<String>,
    #[serde(rename = "listId", default)]
    pub list_id: Option<String>,
    #[serde(rename = "userId", default)]
    pub user_id: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct TimelineBody {
    #[serde(default)]
    pub i: Option<String>,
    #[serde(rename = "listId", default)]
    pub list_id: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(rename = "sinceId", default)]
    pub since_id: Option<String>,
    #[serde(rename = "untilId", default)]
    pub until_id: Option<String>,
    #[serde(rename = "sinceDate", default)]
    pub since_date: Option<i64>,
    #[serde(rename = "untilDate", default)]
    pub until_date: Option<i64>,
}

/// `POST /api/users/lists/create` handler。
pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<CreateBody>>,
) -> Response {
    let body = body.map(|j| j.0).unwrap_or_default();
    let Some(_token) =
        auth::require_scope(&state, &headers, body.i.as_deref(), SCOPE_WRITE_ACCOUNT).await
    else {
        return auth::unauthorized("invalid or revoked token");
    };
    let Some(name) = body
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return bad_request("name is required");
    };
    match repo::user_list::create(state.pool(), name).await {
        Ok(row) => Json(user_list_to_miss(&row, &[])).into_response(),
        Err(err) => {
            tracing::error!(?err, "miauth users/lists/create: insert failed");
            error_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "list creation failed",
            )
        }
    }
}

/// `POST /api/users/lists/list` handler。
pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<ListOnlyBody>>,
) -> Response {
    let body = body.map(|j| j.0).unwrap_or_default();
    let Some(_token) =
        auth::require_scope(&state, &headers, body.i.as_deref(), SCOPE_READ_ACCOUNT).await
    else {
        return auth::unauthorized("invalid or revoked token");
    };
    let rows = match repo::user_list::list_all(state.pool()).await {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(?err, "miauth users/lists/list: query failed");
            return error_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "list query failed",
            );
        }
    };
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let member_ids = repo::user_list::list_member_ids(state.pool(), row.id)
            .await
            .unwrap_or_default();
        out.push(user_list_to_miss(&row, &member_ids));
    }
    Json(out).into_response()
}

/// `POST /api/users/lists/show` handler。
pub async fn show(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<ShowBody>>,
) -> Response {
    let body = body.map(|j| j.0).unwrap_or_default();
    let Some(_token) =
        auth::require_scope(&state, &headers, body.i.as_deref(), SCOPE_READ_ACCOUNT).await
    else {
        return auth::unauthorized("invalid or revoked token");
    };
    let Some(list_id) = parse_list_id(body.list_id.as_deref()) else {
        return error_resp(StatusCode::NOT_FOUND, "NO_SUCH_LIST", "no such list");
    };
    respond_with_list(&state, list_id).await
}

/// `POST /api/users/lists/update` handler。
pub async fn update(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<UpdateBody>>,
) -> Response {
    let body = body.map(|j| j.0).unwrap_or_default();
    let Some(_token) =
        auth::require_scope(&state, &headers, body.i.as_deref(), SCOPE_WRITE_ACCOUNT).await
    else {
        return auth::unauthorized("invalid or revoked token");
    };
    let Some(list_id) = parse_list_id(body.list_id.as_deref()) else {
        return error_resp(StatusCode::NOT_FOUND, "NO_SUCH_LIST", "no such list");
    };
    let Some(name) = body
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return bad_request("name is required");
    };
    match repo::user_list::rename(state.pool(), list_id, name).await {
        Ok(Some(_)) => respond_with_list(&state, list_id).await,
        Ok(None) => error_resp(StatusCode::NOT_FOUND, "NO_SUCH_LIST", "no such list"),
        Err(err) => {
            tracing::error!(?err, list_id, "miauth users/lists/update: failed");
            error_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "list update failed",
            )
        }
    }
}

/// `POST /api/users/lists/delete` handler。
pub async fn delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<ShowBody>>,
) -> Response {
    let body = body.map(|j| j.0).unwrap_or_default();
    let Some(_token) =
        auth::require_scope(&state, &headers, body.i.as_deref(), SCOPE_WRITE_ACCOUNT).await
    else {
        return auth::unauthorized("invalid or revoked token");
    };
    let Some(list_id) = parse_list_id(body.list_id.as_deref()) else {
        return error_resp(StatusCode::NOT_FOUND, "NO_SUCH_LIST", "no such list");
    };
    match repo::user_list::delete_by_id(state.pool(), list_id).await {
        Ok(0) => error_resp(StatusCode::NOT_FOUND, "NO_SUCH_LIST", "no such list"),
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => {
            tracing::error!(?err, list_id, "miauth users/lists/delete: failed");
            error_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "list delete failed",
            )
        }
    }
}

/// `POST /api/users/lists/push` handler (= メンバー追加)。
pub async fn push(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<MemberBody>>,
) -> Response {
    let body = body.map(|j| j.0).unwrap_or_default();
    let Some(_token) =
        auth::require_scope(&state, &headers, body.i.as_deref(), SCOPE_WRITE_ACCOUNT).await
    else {
        return auth::unauthorized("invalid or revoked token");
    };
    let (Some(list_id), Some(user_id)) = (
        parse_list_id(body.list_id.as_deref()),
        parse_list_id(body.user_id.as_deref()),
    ) else {
        return bad_request("listId and userId are required");
    };
    let Some(local) = resolve_self_actor_id(&state).await else {
        return error_resp(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "local actor initialization failed",
        );
    };
    match repo::user_list::add_member(state.pool(), list_id, local, user_id).await {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(AddMemberError::ListNotFound)) => {
            error_resp(StatusCode::NOT_FOUND, "NO_SUCH_LIST", "no such list")
        }
        Ok(Err(AddMemberError::NotFollowing)) => error_resp(
            StatusCode::BAD_REQUEST,
            "NOT_FOLLOWING",
            "you must follow (state=accepted) this user before adding them to a list",
        ),
        Err(err) => {
            tracing::error!(?err, list_id, user_id, "miauth users/lists/push: failed");
            error_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "list member add failed",
            )
        }
    }
}

/// `POST /api/users/lists/pull` handler (= メンバー削除)。
pub async fn pull(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<MemberBody>>,
) -> Response {
    let body = body.map(|j| j.0).unwrap_or_default();
    let Some(_token) =
        auth::require_scope(&state, &headers, body.i.as_deref(), SCOPE_WRITE_ACCOUNT).await
    else {
        return auth::unauthorized("invalid or revoked token");
    };
    let (Some(list_id), Some(user_id)) = (
        parse_list_id(body.list_id.as_deref()),
        parse_list_id(body.user_id.as_deref()),
    ) else {
        return bad_request("listId and userId are required");
    };
    match repo::user_list::remove_member(state.pool(), list_id, user_id).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => {
            tracing::error!(?err, list_id, user_id, "miauth users/lists/pull: failed");
            error_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "list member remove failed",
            )
        }
    }
}

/// `POST /api/notes/user-list-timeline` handler。
///
/// [`crate::miauth::notes::timeline`] (home timeline) と同じ note+renote
/// マージ・カーソル解決ロジックだが、対象を「リストメンバー」に絞る。
#[allow(
    clippy::too_many_lines,
    clippy::similar_names,
    reason = "timeline merge を 1 関数で組む / renoted・renoter は AP 用語 (miauth::notes::timeline と同型)"
)]
pub async fn timeline(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<TimelineBody>>,
) -> Response {
    let body = body.map(|j| j.0).unwrap_or_default();
    let Some(_token) =
        auth::require_scope(&state, &headers, body.i.as_deref(), SCOPE_READ_ACCOUNT).await
    else {
        return auth::unauthorized("invalid or revoked token");
    };
    let Some(list_id) = parse_list_id(body.list_id.as_deref()) else {
        return error_resp(StatusCode::NOT_FOUND, "NO_SUCH_LIST", "no such list");
    };
    if repo::user_list::get_by_id(state.pool(), list_id)
        .await
        .unwrap_or(None)
        .is_none()
    {
        return error_resp(StatusCode::NOT_FOUND, "NO_SUCH_LIST", "no such list");
    }
    let Some(viewer) = resolve_self_actor_id(&state).await else {
        return error_resp(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "local actor initialization failed",
        );
    };

    let limit = body
        .limit
        .unwrap_or(TIMELINE_LIMIT_DEFAULT)
        .clamp(1, TIMELINE_LIMIT_MAX);
    let until_ts = match body.until_id.as_deref() {
        Some(s) => resolve_cursor_ts(&state, s).await,
        None => None,
    }
    .or_else(|| body.until_date.and_then(ms_epoch_to_datetime));
    let since_ts = match body.since_id.as_deref() {
        Some(s) => resolve_cursor_ts(&state, s).await,
        None => None,
    }
    .or_else(|| body.since_date.and_then(ms_epoch_to_datetime));

    let note_entries = match repo::user_list::list_list_timeline_window(
        state.pool(),
        list_id,
        None,
        None,
        since_ts,
        until_ts,
        limit,
    )
    .await
    {
        Ok(v) => v,
        Err(err) => {
            tracing::error!(
                ?err,
                list_id,
                "miauth notes/user-list-timeline: note window failed"
            );
            return error_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "timeline query failed",
            );
        }
    };
    let renote_rows = match repo::announce::list_list_renote_window(
        state.pool(),
        list_id,
        viewer,
        since_ts,
        until_ts,
        limit,
    )
    .await
    {
        Ok(v) => v,
        Err(err) => {
            tracing::error!(
                ?err,
                list_id,
                "miauth notes/user-list-timeline: renote window failed"
            );
            return error_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "timeline query failed",
            );
        }
    };

    let renoted_ids: Vec<i64> = renote_rows.iter().map(|r| r.renoted_note_id).collect();
    let renoter_ids: Vec<i64> = renote_rows.iter().map(|r| r.renoter_actor_id).collect();
    let renoted_entries = repo::note::list_timeline_entries_by_ids(state.pool(), &renoted_ids)
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(
                ?err,
                "notes/user-list-timeline: renoted entries lookup failed; dropping renotes"
            );
            Vec::new()
        });
    let renoter_actors = sakurasato_core::repo::actor::list_by_ids(state.pool(), &renoter_ids)
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(
                ?err,
                "notes/user-list-timeline: renoter actors lookup failed; dropping renotes"
            );
            Vec::new()
        });
    let entry_by_id: std::collections::HashMap<i64, &sakurasato_core::repo::note::TimelineEntry> =
        renoted_entries.iter().map(|e| (e.id, e)).collect();
    let actor_by_id: std::collections::HashMap<i64, &sakurasato_core::model::ActorRow> =
        renoter_actors.iter().map(|a| (a.id, a)).collect();

    let mut all_note_ids: Vec<i64> = note_entries.iter().map(|e| e.id).collect();
    all_note_ids.extend(renoted_ids.iter().copied());
    let summaries = bulk_load_note_summaries(state.pool(), &all_note_ids, viewer).await;
    let empty = NoteSummary {
        reactions: Vec::new(),
        announce: None,
        my_reaction: None,
    };

    let host = &state.config().server.host;

    // actor ごとに emojis map を 1 回解決して共有する (entry ごとの N+1 抑止)。
    let mut actor_ids: Vec<i64> = note_entries.iter().map(|e| e.actor_id).collect();
    actor_ids.extend(renoter_ids.iter().copied());
    let user_emojis = resolve_user_emojis_by_ids(state.pool(), host, &actor_ids).await;

    let mut items: Vec<(DateTime<Utc>, MissNote)> =
        Vec::with_capacity(note_entries.len() + renote_rows.len());
    for e in &note_entries {
        let summary = summaries.get(&e.id).unwrap_or(&empty);
        let emojis = user_emojis.get(&e.actor_id).unwrap_or(&EMPTY_EMOJIS);
        items.push((
            e.published_at,
            timeline_entry_to_miss_note(e, summary, host, emojis),
        ));
    }
    for r in &renote_rows {
        let (Some(entry), Some(actor)) = (
            entry_by_id.get(&r.renoted_note_id),
            actor_by_id.get(&r.renoter_actor_id),
        ) else {
            continue;
        };
        let summary = summaries.get(&entry.id).unwrap_or(&empty);
        let entry_emojis = user_emojis.get(&entry.actor_id).unwrap_or(&EMPTY_EMOJIS);
        let renoted = timeline_entry_to_miss_note(entry, summary, host, entry_emojis);
        // renoter も `user_emojis` (= renoter_ids を含む) から引く ── actor
        // ごと 1 回の解決に畳む (N+1 抑止)。
        let renoter_emojis = user_emojis.get(&actor.id).unwrap_or(&EMPTY_EMOJIS);
        let renoter = from_actor_and_counts(actor, 0, 0, 0, renoter_emojis.clone());
        let created_at = r
            .announce_published_at
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let renote = build_renote_miss_note(
            r.announce_id,
            &r.announce_ap_id,
            &created_at,
            renoter,
            r.renoter_actor_id,
            renoted,
        );
        items.push((r.announce_published_at, renote));
    }

    items.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.id.cmp(&a.1.id)));
    items.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    let notes: Vec<MissNote> = items.into_iter().map(|(_, n)| n).collect();
    Json(notes).into_response()
}

async fn respond_with_list(state: &AppState, list_id: i64) -> Response {
    let Some(row) = (match repo::user_list::get_by_id(state.pool(), list_id).await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!(?err, list_id, "miauth users/lists: lookup failed");
            return error_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "list lookup failed",
            );
        }
    }) else {
        return error_resp(StatusCode::NOT_FOUND, "NO_SUCH_LIST", "no such list");
    };
    let member_ids = repo::user_list::list_member_ids(state.pool(), list_id)
        .await
        .unwrap_or_default();
    Json(user_list_to_miss(&row, &member_ids)).into_response()
}

/// Misskey `id` は string なので、`listId`/`userId` を `i64` に parse する。
fn parse_list_id(s: Option<&str>) -> Option<i64> {
    s.and_then(|t| t.parse::<i64>().ok())
}
