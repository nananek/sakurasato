//! API レベルのエラー表現。
//!
//! 失敗の意味を JSON で本体 (server) に伝えるための共通型。
//! ステータスコードと `reason` 文字列を本体側でログ / メトリクスに使える
//! ようにする (= `error` 文字列はユーザ向け、`reason` は機械可読の安定タグ)。

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// API エラー envelope。
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    /// 機械可読の安定タグ (例: `"blocked_host"`, `"too_large"`)。
    pub reason: &'static str,
    /// 人間向け詳細 (URL や error chain)。秘密値を入れない契約。
    pub message: String,
}

impl ApiError {
    pub fn bad_request(reason: &'static str, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            reason,
            message: message.into(),
        }
    }

    pub fn blocked(reason: &'static str, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            reason,
            message: message.into(),
        }
    }

    pub fn too_large(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            reason: "too_large",
            message: message.into(),
        }
    }

    pub fn unsupported_media(reason: &'static str, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
            reason,
            message: message.into(),
        }
    }

    pub fn upstream(reason: &'static str, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            reason,
            message: message.into(),
        }
    }

    pub fn timeout(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::GATEWAY_TIMEOUT,
            reason: "upstream_timeout",
            message: message.into(),
        }
    }

    pub fn internal(reason: &'static str, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            reason,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // 失敗は必ず構造化 JSON で返す ── server 側 (= `MediaProxyClient`)
        // が `reason` でメトリクスを切れるようにする。
        tracing::warn!(
            status = self.status.as_u16(),
            reason = self.reason,
            message = %self.message,
            "media-proxy: api error",
        );
        let body = Json(json!({
            "error": self.message,
            "reason": self.reason,
        }));
        (self.status, body).into_response()
    }
}
