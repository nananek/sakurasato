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

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::local_api::reactions::{
    ReactionCoreError, create_reaction_core, delete_my_reaction_on_note_core,
};
use crate::miauth::auth;
use crate::miauth::error::error_resp;
use crate::state::AppState;

const SCOPE_WRITE_REACTIONS: &str = "write:reactions";

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
