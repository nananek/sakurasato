//! `POST /api/blocking/create` / `POST /api/blocking/delete` /
//! `POST /api/blocking/list` ── MiAuth 経由のユーザーブロック対応
//! (PR #355 `feature/block-unfollow-domain-block` のフォローアップ、
//! `tmp/plan-miauth-blocking.md` 準拠)。
//!
//! ## 背景
//!
//! PR #355 で実装したユーザーブロック機能は local API (`/api/v1/block*`,
//! TUI/CLI 専用) にのみ露出しており、MiAuth listener には `/api/blocking/*`
//! が一切実装されていなかった。Misskey 互換クライアント (Aria 等) が
//! `MisskeyBlocking.create` (`POST /api/blocking/create` 相当) を呼ぶと
//! axum の default 404 (Misskey 形式のエラーボディではない) が返り、
//! `misskey_dart` の `ApiService.post` が例外を投げて crash する実機報告が
//! あった。本モジュールでそのギャップを埋める。
//!
//! ## 既存パターンの再利用 ([`crate::miauth::following`] と対称)
//!
//! handler は [`crate::block::create_block_core`] / [`crate::block::delete_block_core`]
//! / [`sakurasato_core::repo::block::list_blocked_by_local`] (いずれも PR #355 で
//! `pub` 化済み) を薄くラップするだけ ── `following.rs` が確立した「miauth
//! handler は core 関数をラップするだけ」というパターンをそのまま踏襲する。
//!
//! ## wire 仕様 (clean-room)
//!
//! - <https://api-doc.misskey.io/api/endpoints/blocking/create>
//! - <https://api-doc.misskey.io/api/endpoints/blocking/delete>
//! - <https://api-doc.misskey.io/api/endpoints/blocking/list>
//!
//! `api-doc.misskey.io` の個別ページは SPA レンダリング必須で自動フェッチでは
//! 内容を取得できなかったため、`following/create`・`following/delete` で確認済みの
//! observed wire shape (= 同じ `UserDetailed` 変換を経由するエンドポイント群の慣行)
//! を踏襲する:
//!
//! - `blocking/create` body `{ i, userId }` → 成功時 `blockee` の `UserDetailedNotMe`
//!   (`isBlocking: true`)
//! - `blocking/delete` body `{ i, userId }` → 成功時 `blockee` の `UserDetailedNotMe`
//!   (`isBlocking: false`)
//! - `blocking/list` body `{ i }` → `[{ id, createdAt, blockeeId, blockee }]`
//!   (ページネーション `sinceId`/`untilId`/`limit` は未対応。お一人様サーバの
//!   ブロック件数は実用上小さく、既定で不要と判断。将来必要になれば追加する)
//!
//! エラー: `NO_SUCH_USER` (404、target actor 不在)、`ALREADY_BLOCKING` (409、
//! 自己ブロックの拒否含む ── `create_block_core` は自己ブロックを `Conflict` で
//! 返す)、`NOT_BLOCKING` (404、`delete` で対象をブロックしていない)。
//!
//! ## scope
//!
//! Misskey Hub の公式 permission 一覧 (<https://misskey-hub.net/en/docs/for-developers/api/permission/>)
//! によれば、機能名は `blocking` だが scope 文字列は複数形の **`read:blocks`** /
//! **`write:blocks`** (= `read:following`/`write:following` と同じ命名規則)。
//! endpoint 名 (`blocking`) と scope 名 (`blocks`) が一致しない例外パターンなので
//! 注意 ── `SCOPE_WRITE_FOLLOWING = "write:following"` のような対称な命名には
//! **しない**。
//!
//! ## AGPL discipline
//!
//! [`crate::miauth`] module doc の `[[agpl-discipline-miauth]]` に従い、
//! Misskey の TypeScript handler は読まずに公開 API 仕様のみを一次資料とする。

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use sakurasato_core::model::ActorRow;
use sakurasato_core::repo;
use serde::Deserialize;
use serde_json::json;
use tracing::warn;

use crate::block::{BlockError, UnblockOutcome, create_block_core, delete_block_core};
use crate::follow::FollowTarget;
use crate::miauth::auth;
use crate::miauth::conv::{from_actor_detailed, resolve_user_emojis};
use crate::miauth::error::error_resp;
use crate::miauth::following::{relationships_or_neutral, resolve_local_actor_id};
use crate::state::AppState;

const SCOPE_WRITE_BLOCKS: &str = "write:blocks";
const SCOPE_READ_BLOCKS: &str = "read:blocks";

#[derive(Debug, Deserialize, Default)]
pub struct BlockingBody {
    #[serde(default)]
    pub i: Option<String>,
    /// Misskey は userId を **string** で渡す ([`crate::miauth::following::FollowingBody`]
    /// と同じ)。
    #[serde(rename = "userId", default)]
    pub user_id: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct BlockingListBody {
    #[serde(default)]
    pub i: Option<String>,
}

/// `POST /api/blocking/create` handler。
pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<BlockingBody>>,
) -> Response {
    let body = body.map(|j| j.0).unwrap_or_default();
    let _token_row =
        match auth::require_scope(&state, &headers, body.i.as_deref(), SCOPE_WRITE_BLOCKS).await {
            Ok(t) => t,
            Err(e) => return e.into_response(),
        };
    let Some(target_id) = parse_user_id(body.user_id.as_deref()) else {
        return error_resp(StatusCode::NOT_FOUND, "NO_SUCH_USER", "no such user");
    };

    match create_block_core(&state, FollowTarget::ActorId(target_id)).await {
        Ok(outcome) => Json(build_response(&state, &outcome.target).await).into_response(),
        Err(err) => map_block_error(&err, "create"),
    }
}

/// `POST /api/blocking/delete` handler。
pub async fn delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<BlockingBody>>,
) -> Response {
    let body = body.map(|j| j.0).unwrap_or_default();
    let _token_row =
        match auth::require_scope(&state, &headers, body.i.as_deref(), SCOPE_WRITE_BLOCKS).await {
            Ok(t) => t,
            Err(e) => return e.into_response(),
        };
    let Some(target_id) = parse_user_id(body.user_id.as_deref()) else {
        return error_resp(StatusCode::NOT_FOUND, "NO_SUCH_USER", "no such user");
    };

    // Misskey wire は `userId` を取るが、[`delete_block_core`] は `block_id` を
    // 取る。userId → block_id への解決を間に入れる ([`crate::miauth::following::delete`]
    // と同じ間接パターン) ── viewer は常に local actor なので、
    // `(blocker=local, blocked=userId)` で UNIQUE 行を引く。
    let Some(local) = resolve_local_actor_id(&state).await else {
        return error_resp(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "local actor initialization failed",
        );
    };
    let block_row = match repo::block::get_by_pair(state.pool(), local, target_id).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            return error_resp(
                StatusCode::NOT_FOUND,
                "NOT_BLOCKING",
                "you are not blocking this user",
            );
        }
        Err(err) => {
            tracing::error!(?err, target_id, "miauth blocking/delete: get_by_pair failed");
            return error_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "block lookup failed",
            );
        }
    };

    match delete_block_core(&state, block_row.id).await {
        Ok(outcome) => Json(build_delete_response(&state, &outcome).await).into_response(),
        Err(err) => map_block_error(&err, "delete"),
    }
}

/// `POST /api/blocking/list` handler ── ブロック中の actor を
/// `block.id DESC` (= 最近ブロックした順) で列挙する。ページネーション
/// (`sinceId`/`untilId`/`limit`) は本 follow-up のスコープ外 (module doc 参照)。
pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<BlockingListBody>>,
) -> Response {
    let body = body.map(|j| j.0).unwrap_or_default();
    let _token_row =
        match auth::require_scope(&state, &headers, body.i.as_deref(), SCOPE_READ_BLOCKS).await {
            Ok(t) => t,
            Err(e) => return e.into_response(),
        };
    let Some(local) = resolve_local_actor_id(&state).await else {
        return error_resp(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "local actor initialization failed",
        );
    };

    let rows = match repo::block::list_blocked_by_local(state.pool(), local).await {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(?err, "miauth blocking/list: query failed");
            return error_resp(
                StatusCode::SERVICE_UNAVAILABLE,
                "INTERNAL_ERROR",
                "block list lookup failed",
            );
        }
    };

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let blockee = build_response(&state, &row.actor).await;
        out.push(json!({
            "id": row.block_id.to_string(),
            "createdAt": row.block_created_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "blockeeId": row.actor.id.to_string(),
            "blockee": blockee,
        }));
    }
    Json(out).into_response()
}

/// `blocking/delete` の成功 body ── unblock した相手 user の `UserDetailedNotMe`。
/// [`crate::miauth::following::build_delete_response`] と同型。
async fn build_delete_response(state: &AppState, outcome: &UnblockOutcome) -> serde_json::Value {
    // target_ap_id から actor row を再 fetch ── unblock 後でも actor 行は残っている
    // (= follow.rs::build_delete_response と同じ理由)。
    match repo::actor::get_by_ap_id(state.pool(), &outcome.target_ap_id).await {
        Ok(Some(actor)) => build_response(state, &actor).await,
        _ => json!({}),
    }
}

/// Misskey `UserDetailedNotMe` 相当のレスポンス body を組み立てる。
/// `blocking/create`(ブロック直後)・`blocking/delete`(アンブロック直後)・
/// `blocking/list`(一覧の各行) が共有する
/// ([`crate::miauth::following::build_create_response`] と同型)。
async fn build_response(state: &AppState, actor: &ActorRow) -> serde_json::Value {
    let (followers, following, notes) =
        crate::miauth::counts::counts_for_actor(state, actor).await;
    let (rel, is_blocking, is_blocked) = relationships_or_neutral(state, actor.id).await;
    let emojis = resolve_user_emojis(state.pool(), &state.config().server.host, actor).await;
    from_actor_detailed(actor, followers, following, notes, rel, is_blocking, is_blocked, emojis)
}

fn parse_user_id(s: Option<&str>) -> Option<i64> {
    s.and_then(|t| t.parse::<i64>().ok())
}

/// `BlockError` を Misskey 互換 error response にマップする。
/// [`crate::miauth::following::map_follow_error`] と対称の構成。
fn map_block_error(err: &BlockError, op: &str) -> Response {
    match err {
        BlockError::BadRequest(msg) => {
            warn!(error = %msg, op, "miauth blocking: bad request");
            error_resp(StatusCode::BAD_REQUEST, "INVALID_PARAM", msg)
        }
        BlockError::BadGateway(msg) => {
            warn!(error = %msg, op, "miauth blocking: upstream failure");
            error_resp(StatusCode::BAD_GATEWAY, "FEDERATION_ERROR", msg)
        }
        BlockError::NotFound(msg) => {
            warn!(error = %msg, op, "miauth blocking: not found");
            error_resp(StatusCode::NOT_FOUND, "NO_SUCH_USER", msg)
        }
        BlockError::Unavailable(msg) => {
            warn!(error = %msg, op, "miauth blocking: unavailable");
            error_resp(StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE", msg)
        }
        BlockError::Conflict(msg) => {
            warn!(error = %msg, op, "miauth blocking: conflict");
            error_resp(StatusCode::CONFLICT, "ALREADY_BLOCKING", msg)
        }
        BlockError::Forbidden(msg) => {
            warn!(error = %msg, op, "miauth blocking: forbidden");
            error_resp(StatusCode::FORBIDDEN, "PERMISSION_DENIED", msg)
        }
        BlockError::Internal(e) => {
            tracing::error!(error = ?e, op, "miauth blocking: internal failure");
            error_resp(
                StatusCode::SERVICE_UNAVAILABLE,
                "INTERNAL_ERROR",
                "block operation failed; check server logs",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    // DB を要する経路 (require_scope / repo lookup) は
    // `crates/server/tests/miauth_write_pg.rs` の sqlx::test 統合テストで
    // カバーする (= following.rs / following_requests.rs と同じ流儀)。ここでは
    // pure な body parsing だけ確認する。
    use super::*;

    #[test]
    fn blocking_body_parses_string_user_id() {
        let body: BlockingBody =
            serde_json::from_value(json!({"i": "tok", "userId": "42"})).unwrap();
        assert_eq!(body.user_id.as_deref(), Some("42"));
    }

    #[test]
    fn blocking_body_missing_user_id_parses_to_none() {
        let body: BlockingBody = serde_json::from_value(json!({"i": "tok"})).unwrap();
        assert_eq!(body.user_id, None);
    }
}
