//! Inbox endpoints.
//!
//! 受信した `ActivityPub` アクティビティの受け口。`SignedInboxBody` extractor が
//! HTTP 署名検証 (PR1) と remote actor fetch (PR2) を済ませた上で本 handler に
//! 制御を渡す。
//!
//! 本 handler は body を [`crate::dispatch`] に流し、F3 actor 一致検証と
//! `type` 別ハンドラ (Follow / Accept / Reject / Like / `EmojiReact` / Undo) を
//! 実行する。`Create`/`Note` / `Update` / `Delete` / `Move` / `Announce` は
//! 後続 PR で順次。
//!
//! 未対応 Activity 型は 202 で受け流す ── 相手の再送ループに乗せないため。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use sakurasato_core::repo;

use crate::dispatch::{self, DispatchError};
use crate::extract::SignedInboxBody;
use crate::state::AppState;

pub(crate) async fn shared_inbox(
    State(state): State<AppState>,
    signed: SignedInboxBody,
) -> Result<Response, DispatchError> {
    handle(&state, &signed, None).await
}

pub(crate) async fn user_inbox(
    State(state): State<AppState>,
    Path(name): Path<String>,
    signed: SignedInboxBody,
) -> Result<Response, DispatchError> {
    // recipient がローカルユーザでなければ 404 ── 存在しないユーザ宛 inbox を
    // shared と同一扱いにしない。署名検証 (extractor) は通過済みだが、dispatch
    // 前に弾くことで未知宛ての受信処理・ログ混入を防ぐ。
    let host = &state.config().server.host;
    let is_local_user = match repo::actor::get_by_username_host(state.pool(), &name, host).await {
        Ok(Some(row)) => row.is_local,
        Ok(None) => false,
        Err(e) => {
            return Err(DispatchError::Internal(e.into()));
        }
    };
    if !is_local_user {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    handle(&state, &signed, Some(name.as_str())).await
}

async fn handle(
    state: &AppState,
    signed: &SignedInboxBody,
    recipient: Option<&str>,
) -> Result<Response, DispatchError> {
    tracing::info!(
        actor = %signed.actor.ap_id,
        scheme = ?signed.scheme,
        key_kind = ?signed.key_kind,
        recipient = ?recipient,
        body_size = signed.body.len(),
        "inbox accepted",
    );
    dispatch::dispatch(state, &signed.actor, &signed.body).await
}
