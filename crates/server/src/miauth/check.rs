//! `POST /api/miauth/{uuid}/check` ── Misskey `MiAuth` クライアントの polling
//! endpoint (= M14 #158, 親 issue #150)。
//!
//! ## 状態 → レスポンス
//!
//! | session.state | レスポンス |
//! |---|---|
//! | `pending`  | `200 {ok: false, token: null}` (= polling 継続) |
//! | `approved` | **token 発行 + consumed への CAS** → `200 {ok: true, token, user}` |
//! | `consumed` | 冪等経路 ── `raw_token_for_polling` 列を読み戻し `200 {ok: true, token, user}` を返す |
//! | `rejected` / `expired` / 不在 / 不正 UUID | `404` |
//!
//! `approved` → `consumed` の遷移は **`UPDATE ... WHERE state = 'approved'`**
//! の **アトミック CAS** で 1 つしか勝てない。並行 polling のレース時に負け
//! た側は `get_session` で **consumed** + `raw_token_for_polling` を読み直し
//! 同じ raw を返す経路に倒れる ── どの client も同じ token を観測する。
//!
//! ## CLI との責務分担
//!
//! M14 #157 → #158 で CLI [`crate::miauth_cli::run_approve`] は **state CAS
//! のみ** に縮退した:
//!
//! - **CLI**: `pending` → `approved` の CAS だけ (= ユーザが「許可した」事実
//!   を server に伝える)。raw token は **発行しない**
//! - **check handler (本 module)**: `approved` を見たら raw 発行 + `miauth_token`
//!   INSERT + session を `consumed` に CAS + raw を `raw_token_for_polling`
//!   列に保管 → レスポンスで client に渡す
//!
//! この役割分担で Misskey wire 互換性 (= polling で `{token, user}` を取る)
//! と Sakurasato 設計 (= 認可は CLI でしか出せない) が両立する。

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use sakurasato_core::model::{MiAuthSessionRow, MiAuthSessionState};
use sakurasato_core::repo;
use serde::Serialize;
use serde_json::json;
use uuid::Uuid;

use crate::miauth::conv::{MissUser, from_actor_and_counts};
use crate::state::AppState;

/// `POST /api/miauth/{uuid}/check` のレスポンス body。
///
/// Misskey wire shape は `{ok: bool, token: string|null, user: MissUser|null}` 形式。
/// `ok = false` のときは `token` / `user` 両方 null (= pending)。
/// `ok = true` のときは `token` 文字列 + `user` `MissUser`。
#[derive(Debug, Serialize)]
struct CheckResponse {
    ok: bool,
    token: Option<String>,
    user: Option<MissUser>,
}

/// `POST /api/miauth/{uuid}/check` handler。
pub async fn handle(
    State(state): State<AppState>,
    Path(uuid_str): Path<String>,
) -> Response {
    let Ok(uuid) = Uuid::parse_str(&uuid_str) else {
        return not_found("invalid UUID");
    };

    // best-effort expire sweep: 過期 pending を expired に倒してから状態判定に
    // 入る。失敗しても check 自体は試みる (= sweep が落ちても polling は通す)。
    let _ = repo::miauth::expire_old_sessions(state.pool()).await;

    let session = match repo::miauth::get_session(state.pool(), uuid).await {
        Ok(Some(row)) => row,
        Ok(None) => return not_found("session not found"),
        Err(err) => {
            tracing::error!(?err, %uuid, "miauth_session get failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    match session.state_enum() {
        Some(MiAuthSessionState::Pending) | None => {
            // polling 継続を促す。`ok = false` で「まだ」を表現。
            Json(CheckResponse {
                ok: false,
                token: None,
                user: None,
            })
            .into_response()
        }
        Some(MiAuthSessionState::Approved) => {
            handle_approved(&state, &session).await
        }
        Some(MiAuthSessionState::Consumed) => {
            handle_consumed(&state, &session).await
        }
        Some(MiAuthSessionState::Rejected | MiAuthSessionState::Expired) => {
            not_found("session is not pending")
        }
    }
}

/// `approved` 状態の session に対する初回 check ── token 発行 + consumed への
/// CAS + raw token を session 列に保管。レース時の負け側は `consumed` 経路に
/// fall through する。
async fn handle_approved(state: &AppState, session: &MiAuthSessionRow) -> Response {
    let raw = crate::token::generate_raw();
    let token_hash = crate::token::hash(&raw);
    let token_row = match repo::miauth::insert_token(
        state.pool(),
        repo::miauth::NewMiAuthToken {
            name: session.app_name.clone(),
            token_hash,
            permissions: session.permissions.0.clone(),
        },
    )
    .await
    {
        Ok(row) => row,
        Err(err) => {
            tracing::error!(?err, uuid = %session.uuid, "miauth_token INSERT failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let cas = match repo::miauth::mark_session_consumed_with_raw(
        state.pool(),
        session.uuid,
        token_row.id,
        &raw,
    )
    .await
    {
        Ok(n) => n,
        Err(err) => {
            tracing::error!(?err, uuid = %session.uuid, "mark_session_consumed_with_raw failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    if cas == 0 {
        // レース時に別 polling が CAS 勝ち & raw を書き終えた ── token_row は
        // 既に INSERT 済みなので、これは「重複 token row」が生まれた状態。
        // session.issued_token_id は勝者の token を指しているはず。**今 INSERT
        // した token を revoke** して状態を綺麗にしてから、consumed 経路に fall
        // through する。
        if let Err(err) = repo::miauth::delete_token_by_id(state.pool(), token_row.id).await {
            tracing::warn!(
                ?err,
                stale_token_id = token_row.id,
                "failed to revoke stale miauth_token from CAS race; manual cleanup may be needed"
            );
        }
        return match repo::miauth::get_session(state.pool(), session.uuid).await {
            Ok(Some(row)) => handle_consumed(state, &row).await,
            _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
    }

    let user = match build_miss_user(state).await {
        Ok(u) => u,
        Err(resp) => return resp,
    };
    Json(CheckResponse {
        ok: true,
        token: Some(raw),
        user: Some(user),
    })
    .into_response()
}

/// `consumed` 状態の session に対する 2 回目以降の check ── `raw_token_for_polling`
/// 列から raw を読み戻して同じ token を返す (冪等)。grace sweep 後に raw が
/// NULL に倒れているケースでは `token: null` を返す ── client は手元の token
/// を保持して使い続ける責務がある。
async fn handle_consumed(state: &AppState, session: &MiAuthSessionRow) -> Response {
    let user = match build_miss_user(state).await {
        Ok(u) => u,
        Err(resp) => return resp,
    };
    Json(CheckResponse {
        ok: true,
        token: session.raw_token_for_polling.clone(),
        user: Some(user),
    })
    .into_response()
}

/// `404` レスポンス。Misskey 慣行で body に `error.code` / `error.message` を
/// 載せる (= client が UI に表示するため)。
fn not_found(reason: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "application/json")],
        Json(json!({
            "ok": false,
            "error": {
                "code": "NOT_FOUND",
                "message": reason,
            },
        })),
    )
        .into_response()
}

/// local actor + 集計 count → `MissUser`。失敗 (= local actor 不在 / DB エラー)
/// は 500 を返す。
async fn build_miss_user(state: &AppState) -> Result<MissUser, Response> {
    let host = &state.config().server.host;
    let user = &state.config().server.user;
    let actor = match sakurasato_core::repo::actor::get_by_username_host(state.pool(), user, host)
        .await
    {
        Ok(Some(row)) if row.is_local => row,
        Ok(_) => {
            tracing::error!(host, user, "local actor not found for /api/miauth/check");
            return Err(StatusCode::INTERNAL_SERVER_ERROR.into_response());
        }
        Err(err) => {
            tracing::error!(?err, "local actor lookup failed for /api/miauth/check");
            return Err(StatusCode::INTERNAL_SERVER_ERROR.into_response());
        }
    };
    let followers = sakurasato_core::repo::follow::count_followers(state.pool(), actor.id)
        .await
        .unwrap_or(0);
    let following = sakurasato_core::repo::follow::count_following(state.pool(), actor.id)
        .await
        .unwrap_or(0);
    let notes = sakurasato_core::repo::note::count_local(state.pool())
        .await
        .unwrap_or(0);
    Ok(from_actor_and_counts(&actor, followers, following, notes))
}
