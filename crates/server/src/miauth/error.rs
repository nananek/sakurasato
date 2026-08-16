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
//! ## Misskey wire の完全エンベロープ (fix/miauth-follow-failure)
//!
//! Misskey 公式エラー body は `{"error": {"code", "message", "id", "kind"}}` の
//! 4 フィールドを持ち、**`id` は misskey-dart の `MisskeyException.fromJson` が
//! required (`id: json['id'] as String`) で読む**。本サーバは従来 `code` /
//! `message` しか返していなかったため、クライアント側で parse が throw → raw
//! `DioException` に倒れてエラー表示が壊れていた (報告の二次要因)。全 `MiAuth`
//! エラーに `id` (UUID v4) と `kind` を追加して解決する:
//!
//! - `id`: 毎回 `uuid::Uuid::new_v4()` で生成 (Misskey 公式同様、追跡用)。
//! - `kind`: fork の `MisskeyExceptionKind` enum 準拠 ── 5xx = `"server"`、
//!   `PERMISSION_DENIED` (= scope 不足 / 所有権拒否) = `"permission"`、
//!   それ以外の 4xx = `"client"`。
//!
//! 既存テストは `v["error"]["code"]` 等を assert しているだけで、フィールド
//! 追加は後方互換 (= 壊れない)。

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use uuid::Uuid;

/// Misskey wire のエラー body (`{"error": {"code", "message"}}`) を任意の
/// status で返す汎用ヘルパー。`id` (UUID v4) と `kind` を自動付与する。
pub fn error_resp(status: StatusCode, code: &str, message: &str) -> Response {
    let kind = error_kind(status, code);
    (
        status,
        Json(json!({
            "error": {
                "code": code,
                "message": message,
                "id": Uuid::new_v4().to_string(),
                "kind": kind,
            }
        })),
    )
        .into_response()
}

/// `error_resp` / `unauthorized` / `forbidden` が body に載せる `kind` の値。
///
/// fork の `MisskeyExceptionKind` enum に合わせる (= `enumDecodeNullable` が
/// 読むのは小文字文字列): 5xx = `"server"`、`PERMISSION_DENIED` = `"permission"`、
/// それ以外の 4xx = `"client"`。
pub fn error_kind(status: StatusCode, code: &str) -> &'static str {
    if status.is_server_error() {
        "server"
    } else if code == "PERMISSION_DENIED" {
        "permission"
    } else {
        "client"
    }
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

// NOTE: 汎用 404 / 401 helper は意図的に置かない。404 は各 endpoint が
// `error_resp(NOT_FOUND, "<NO_SUCH_*>", ..)` で **固有 code** (`NO_SUCH_USER` /
// `NO_SUCH_NOTE` / `NO_SUCH_FILE` 等) を明示し、401 は `WWW-Authenticate` ヘッダ
// 付きの [`crate::miauth::auth::unauthorized`] に集約する。generic な
// not_found/unauthorized を置くと固有 code を握り潰す誤用を招くため (#197)。

#[cfg(test)]
mod tests {
    use super::*;

    async fn read_error(resp: Response) -> serde_json::Value {
        use http_body_util::BodyExt;
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&body).unwrap()
    }

    /// 4xx は `kind = "client"`、`id` は非空の文字列 (= misskey-dart の
    /// required `id` を満たす)。フィールド追加は既存 `code` / `message` を
    /// 壊さない。
    #[tokio::test]
    async fn error_resp_client_kind_includes_id() {
        let resp = error_resp(StatusCode::NOT_FOUND, "NO_SUCH_USER", "no such user");
        let v = read_error(resp).await;
        assert_eq!(v["error"]["code"], "NO_SUCH_USER");
        assert_eq!(v["error"]["message"], "no such user");
        assert_eq!(v["error"]["kind"], "client");
        let id = v["error"]["id"].as_str().expect("id must be a string");
        assert!(!id.is_empty(), "id must be non-empty UUID");
    }

    /// 5xx は `kind = "server"`。
    #[tokio::test]
    async fn error_resp_server_kind() {
        let resp = error_resp(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", "boom");
        let v = read_error(resp).await;
        assert_eq!(v["error"]["kind"], "server");
    }

    /// `PERMISSION_DENIED` (= scope 不足 / 所有権拒否) は `kind = "permission"`
    /// (= fork の `MisskeyExceptionKind.permission` に一致)。
    #[tokio::test]
    async fn error_resp_permission_kind() {
        let resp = error_resp(StatusCode::FORBIDDEN, "PERMISSION_DENIED", "nope");
        let v = read_error(resp).await;
        assert_eq!(v["error"]["kind"], "permission");
    }
}
