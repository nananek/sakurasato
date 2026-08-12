//! `POST /api/notes/reactions/create` / `POST /api/notes/reactions/delete`
//! (= M14 #160, 親 issue #150)。
//!
//! Misskey 互換 ── ローカル user が自分以外の Note にもリアクションを付けられる。
//!
//! ## wire 仕様 (clean-room)
//!
//! - <https://api-doc.misskey.io/api/endpoints/notes/reactions/create>
//! - <https://api-doc.misskey.io/api/endpoints/notes/reactions/delete>
//!
//! observed body:
//!
//! - create: `{ i: <token>, noteId: "<string>", reaction: "<string>" }`
//! - delete: `{ i: <token>, noteId: "<string>" }` (= note 単位で「自分の reaction」を消す)
//!
//! `reaction` フィールドは Unicode 絵文字 / `:shortcode:` (ローカル) /
//! `:shortcode@host:` (リモート) の 3 形式を受理する仕様。本 PR では既存
//! [`crate::local_api::reactions`] の core ロジックを共有し、Unicode + ローカル
//! `:shortcode:` の 2 形だけ対応する。`:shortcode@host:` (リモート絵文字反応) は
//! 既存 core が 400 で弾く (= M9 以降の remote emoji 自動学習の課題と一致)。
//!
//! ## 成功 response
//!
//! Misskey 公式は **204 No Content** で返す慣行 (= body なし)。`misskey-py` も
//! 204 を success として扱う。Sakurasato も 204 で揃える。
//!
//! ## scope
//!
//! - create: **`write:reactions`** scope
//! - delete: **`write:reactions`** scope (= Misskey 仕様で create/delete 同じ)

use std::collections::HashMap;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use sakurasato_core::repo;
use serde::Deserialize;
use serde_json::{Value as JsonValue, json};

use crate::local_api::reactions::{
    ReactionCoreError, create_reaction_core, delete_my_reaction_on_note_core,
};
use crate::miauth::auth;
use crate::miauth::conv::{from_actor_and_counts, resolve_user_emojis};
use crate::miauth::error::error_resp;
use crate::miauth::notes::{resolve_self_actor_id, viewer_can_view_entry};
use crate::state::AppState;

const SCOPE_WRITE_REACTIONS: &str = "write:reactions";
/// `notes/reactions` (read) は `read:account` scope。`notes/show` と揃える。
const SCOPE_READ_ACCOUNT: &str = "read:account";

/// `notes/reactions` の limit 既定/上限 (Misskey 既定は 10)。
const LIST_LIMIT_DEFAULT: i64 = 10;
const LIST_LIMIT_MAX: i64 = 100;

#[derive(Debug, Deserialize, Default)]
pub struct CreateReactionBody {
    #[serde(default)]
    pub i: Option<String>,
    #[serde(rename = "noteId", default)]
    pub note_id: Option<String>,
    /// Unicode 絵文字 / `:shortcode:` / `:shortcode@host:` (= 後者は未対応)。
    #[serde(default)]
    pub reaction: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct DeleteReactionBody {
    #[serde(default)]
    pub i: Option<String>,
    #[serde(rename = "noteId", default)]
    pub note_id: Option<String>,
}

/// `POST /api/notes/reactions/create` handler。
pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<CreateReactionBody>>,
) -> Response {
    let body = body.map(|j| j.0).unwrap_or_default();
    let Some(_token) =
        auth::require_scope(&state, &headers, body.i.as_deref(), SCOPE_WRITE_REACTIONS).await
    else {
        return auth::unauthorized("invalid or revoked token");
    };
    let Some(note_id) = body.note_id.as_deref().and_then(|s| s.parse::<i64>().ok()) else {
        return error_resp(StatusCode::NOT_FOUND, "NO_SUCH_NOTE", "no such note");
    };
    let Some(reaction) = body.reaction.as_deref().filter(|s| !s.is_empty()) else {
        return error_resp(
            StatusCode::BAD_REQUEST,
            "INVALID_PARAM",
            "reaction is required",
        );
    };

    match create_reaction_core(&state, note_id, reaction).await {
        Ok(_outcome) => {
            // Misskey wire は 204 No Content。`misskey-py` は 204 を success
            // と扱う。空 body で返す。
            (StatusCode::NO_CONTENT, ()).into_response()
        }
        Err(err) => map_reaction_core_err(&err),
    }
}

/// `POST /api/notes/reactions/delete` handler。
pub async fn delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<DeleteReactionBody>>,
) -> Response {
    let body = body.map(|j| j.0).unwrap_or_default();
    let Some(_token) =
        auth::require_scope(&state, &headers, body.i.as_deref(), SCOPE_WRITE_REACTIONS).await
    else {
        return auth::unauthorized("invalid or revoked token");
    };
    let Some(note_id) = body.note_id.as_deref().and_then(|s| s.parse::<i64>().ok()) else {
        return error_resp(StatusCode::NOT_FOUND, "NO_SUCH_NOTE", "no such note");
    };

    match delete_my_reaction_on_note_core(&state, note_id).await {
        Ok(_outcome) => (StatusCode::NO_CONTENT, ()).into_response(),
        Err(err) => map_reaction_core_err(&err),
    }
}

/// `POST /api/notes/reactions` body (= reaction 一覧、Misskey `notes/reactions`)。
///
/// wire: <https://api-doc.misskey.io/api/endpoints/notes/reactions>
/// `{ i, noteId(必須), type?, limit?, offset?, sinceId?, untilId? }`。
#[derive(Debug, Deserialize, Default)]
pub struct ListReactionsBody {
    #[serde(default)]
    pub i: Option<String>,
    #[serde(rename = "noteId", default)]
    pub note_id: Option<String>,
    /// reaction 種別フィルタ (= content 完全一致)。`:foo:` / `:foo@host:` / Unicode。
    #[serde(rename = "type", default)]
    pub type_: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
    #[serde(rename = "sinceId", default)]
    pub since_id: Option<String>,
    #[serde(rename = "untilId", default)]
    pub until_id: Option<String>,
}

/// `POST /api/notes/reactions` handler ── note 1 件の個別 reaction を
/// `NoteReaction[]` (`{id, createdAt, user, type}`) で返す (Aria の reaction 詳細)。
pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<ListReactionsBody>>,
) -> Response {
    let body = body.map(|j| j.0).unwrap_or_default();
    let Some(_token) =
        auth::require_scope(&state, &headers, body.i.as_deref(), SCOPE_READ_ACCOUNT).await
    else {
        return auth::unauthorized("invalid or revoked token");
    };
    let Some(note_id) = body.note_id.as_deref().and_then(|s| s.parse::<i64>().ok()) else {
        return error_resp(StatusCode::NOT_FOUND, "NO_SUCH_NOTE", "no such note");
    };

    // note の存在 + 可視性チェック (= 見えない note の reaction を漏らさない)。
    let entry = match repo::note::get_timeline_entry_by_id(state.pool(), note_id).await {
        Ok(Some(e)) => e,
        Ok(None) => return error_resp(StatusCode::NOT_FOUND, "NO_SUCH_NOTE", "no such note"),
        Err(err) => {
            tracing::error!(?err, note_id, "miauth notes/reactions: note lookup failed");
            return error_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "note lookup failed",
            );
        }
    };
    let Some(viewer) = resolve_self_actor_id(&state).await else {
        return error_resp(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "local actor initialization failed",
        );
    };
    if !viewer_can_view_entry(&state, &entry, viewer).await {
        return error_resp(StatusCode::NOT_FOUND, "NO_SUCH_NOTE", "no such note");
    }

    let limit = body
        .limit
        .unwrap_or(LIST_LIMIT_DEFAULT)
        .clamp(1, LIST_LIMIT_MAX);
    let offset = body.offset.unwrap_or(0).max(0);
    let type_filter = body.type_.as_deref().filter(|s| !s.is_empty());
    let since_id = body.since_id.as_deref().and_then(|s| s.parse::<i64>().ok());
    let until_id = body.until_id.as_deref().and_then(|s| s.parse::<i64>().ok());

    let rows = match repo::reaction::list_for_note(
        state.pool(),
        note_id,
        type_filter,
        since_id,
        until_id,
        offset,
        limit,
    )
    .await
    {
        Ok(v) => v,
        Err(err) => {
            tracing::error!(?err, note_id, "miauth notes/reactions: list failed");
            return error_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "reaction list query failed",
            );
        }
    };

    // reactor の MissUser を組み立てる。同一 actor の連投を引き直さないよう cache。
    let mut user_cache: HashMap<i64, JsonValue> = HashMap::new();
    let host = state.config().server.host.clone();
    let mut out: Vec<JsonValue> = Vec::with_capacity(rows.len());
    for row in &rows {
        let user = if let Some(cached) = user_cache.get(&row.actor_id) {
            cached.clone()
        } else {
            let built = match repo::actor::get_by_id(state.pool(), row.actor_id).await {
                Ok(Some(actor)) => {
                    let emojis = resolve_user_emojis(state.pool(), &host, &actor).await;
                    serde_json::to_value(from_actor_and_counts(&actor, 0, 0, 0, emojis))
                        .unwrap_or(JsonValue::Null)
                }
                _ => JsonValue::Null,
            };
            user_cache.insert(row.actor_id, built.clone());
            built
        };
        // reactor を引けなかった行は wire 仕様上 `user` 必須なので落とす
        // (= NoteReaction.user は non-null。null を返すと Dart 側 parse 例外)。
        if user.is_null() {
            continue;
        }
        out.push(json!({
            "id": row.id.to_string(),
            "createdAt": row.created_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "user": user,
            // type は DB 保存形そのまま (= `:foo@host:` 形は #242 で note の
            // reactionEmojis key と一致するので Aria が解決できる)。
            "type": row.content,
        }));
    }

    Json(out).into_response()
}

/// `ReactionCoreError` を Misskey 互換 error response にマップする。
fn map_reaction_core_err(err: &ReactionCoreError) -> Response {
    match err {
        ReactionCoreError::BadRequest(msg) => {
            error_resp(StatusCode::BAD_REQUEST, "INVALID_PARAM", msg)
        }
        ReactionCoreError::NoteNotFound => {
            error_resp(StatusCode::NOT_FOUND, "NO_SUCH_NOTE", "no such note")
        }
        ReactionCoreError::ReactionNotFound => error_resp(
            StatusCode::NOT_FOUND,
            "NOT_REACTED",
            "you have not reacted to this note",
        ),
        ReactionCoreError::EmojiNotFound => error_resp(
            StatusCode::NOT_FOUND,
            "NO_SUCH_EMOJI",
            "local emoji not found",
        ),
        ReactionCoreError::NotOwned => error_resp(
            StatusCode::FORBIDDEN,
            "PERMISSION_DENIED",
            "reaction not owned by you",
        ),
        ReactionCoreError::LocalActorMissing => error_resp(
            StatusCode::SERVICE_UNAVAILABLE,
            "UNAVAILABLE",
            "local actor not initialized; run `sakurasato init`",
        ),
        ReactionCoreError::Internal => error_resp(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "reaction operation failed; check server logs",
        ),
    }
}
