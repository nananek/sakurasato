//! `Authorization: Bearer <token>` を検証するルータミドルウェア。
//!
//! 失敗時は 401 + `WWW-Authenticate: Bearer realm="sakurasato"` を返し、
//! 本文に JSON エラーを載せる。成功時は対応する [`ApiTokenRow`] をリクエスト
//! 拡張に格納してハンドラから参照できるようにし、`last_used_at` を
//! best-effort で更新する (失敗してもリクエストは通す)。

use axum::Json;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sakurasato_core::model::ApiTokenRow;
use sakurasato_core::repo;
use serde_json::json;

use crate::state::AppState;
use crate::token;

pub async fn require_token(
    State(state): State<AppState>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    let Some(raw) = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(token::parse_bearer)
    else {
        return unauthorized("missing or malformed Authorization header");
    };

    let hash = token::hash(raw);
    let row = match repo::api_token::find_by_hash(state.pool(), &hash).await {
        Ok(Some(row)) => row,
        Ok(None) => return unauthorized("invalid token"),
        Err(err) => {
            tracing::error!(?err, "api_token lookup failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // last_used_at は **best-effort**: UPDATE が失敗 (pool exhaustion / DB
    // 一時不通) してもリクエスト本体は通すべきなので、別タスクに退避させて
    // エラーは warn ログだけ残す。`AppState` は `Arc` ベースで clone 安価。
    let task_state = state.clone();
    let token_id = row.id;
    tokio::spawn(async move {
        if let Err(err) = repo::api_token::mark_used(task_state.pool(), token_id).await {
            tracing::warn!(?err, token_id, "api_token mark_used failed");
        }
    });

    req.extensions_mut().insert(row);
    next.run(req).await
}

fn unauthorized(reason: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, r#"Bearer realm="sakurasato""#)],
        Json(json!({"error": reason})),
    )
        .into_response()
}

/// Convenience for handlers: pull the authenticated token row out of the
/// request extensions. Only call this from handlers gated by
/// [`require_token`] — otherwise it returns `None`.
#[allow(dead_code)] // M4 PR2 で timeline ハンドラから使う予定
pub fn authenticated_token(req: &Request<Body>) -> Option<&ApiTokenRow> {
    req.extensions().get::<ApiTokenRow>()
}
