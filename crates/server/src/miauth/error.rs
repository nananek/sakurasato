//! `MiAuth` (Misskey 互換 API) の共通エラーレスポンス。
//!
//! Misskey の wire 仕様では、エラーは HTTP status に加えて body に
//! `{"error": {"code": "<UPPER_SNAKE>", "message": "<人間可読>"}}` を載せる。
//! Aria / Milktea / `MissRirica` 等のクライアントはこの `code` / `message` を
//! 見てユーザ提示・分岐・ログ表示するので、**bare 500 (body 無し) を返すと
//! クライアント側で「不明なエラー」になりデバッグ不能**になる。
//!
//! そこで `MiAuth` ハンドラのエラーはすべてこのモジュールのヘルパー経由で
//! 返し、必ず `code` + `message` を含める。`message` は内部実装 (SQL 文 /
//! スタックトレース等) を漏らさない範囲で「どの操作が失敗したか」が分かる
//! 文言にする ── 詳細は `tracing::error!` 側に残す。
//!
//! 注: 本実装は Misskey 公式が付ける `error.id` (UUID) は載せていない。
//! クライアントは主に `code` / `message` を使うため実害はないが、必要に
//! なれば [`error_resp`] に追加する。

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// Misskey wire のエラー body (`{"error": {"code", "message"}}`) を任意の
/// status で返す汎用ヘルパー。
pub fn error_resp(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({ "error": { "code": code, "message": message } })),
    )
        .into_response()
}

/// 500 Internal Server Error。`code = "INTERNAL_ERROR"`。
///
/// DB エラー等の内部失敗で使う。`message` は「どの操作が失敗したか」が
/// 分かる短い文言にする (例: `"failed to load MiAuth session"`)。
pub fn internal_error(message: &str) -> Response {
    error_resp(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", message)
}

/// 400 Bad Request。`code = "INVALID_PARAM"`。
pub fn bad_request(message: &str) -> Response {
    error_resp(StatusCode::BAD_REQUEST, "INVALID_PARAM", message)
}

/// 404 Not Found。`code = "NOT_FOUND"`。
pub fn not_found(message: &str) -> Response {
    error_resp(StatusCode::NOT_FOUND, "NOT_FOUND", message)
}

/// 401 Unauthorized。`code = "AUTHENTICATION_FAILED"`。
pub fn unauthorized(message: &str) -> Response {
    error_resp(StatusCode::UNAUTHORIZED, "AUTHENTICATION_FAILED", message)
}
