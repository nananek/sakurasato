//! `GET /api/v1/media/proxy?url=...&variant=...` ── TUI 向けの画像プロキシ。
//!
//! TUI (ホスト端末プロセス) は本サーバの local API 経由でアバター等を取り、
//! さらにその先 (= media-proxy コンテナ) で外部 GET と画像デコードを行う
//! [[Issue #36]]。これで:
//!
//! - TUI ホストプロセスから直接外部 GET が出なくなる
//!   (= ホスト LAN / クラウド IMDS への SSRF リスクを縮小)
//! - server 本体は外部 URL から取ったバイト列を一切デコードしない
//!   (= [`crate::media_proxy_client::MediaProxyClient`] が WebP 再エンコード
//!   済みバイト列を返してくる)
//!
//! ## レスポンス
//!
//! - 成功: `200 OK`、`Content-Type` は media-proxy が返した値 (`image/webp`)、
//!   本文は変換後のバイト列。
//! - 失敗: media-proxy の `reason` を見て分類:
//!   - `invalid_url` / `unsupported_scheme` / `empty_body` → `400`
//!   - `loopback` / `private` / 等 SSRF 系 → `400` (= クライアントの責)
//!   - `too_large` → `413`
//!   - `unsupported_format` / `decode_failed` 等 → `502` (= 上流が壊れている)
//!   - `upstream_http` / `transport` / `connect` → `502`
//!   - `upstream_timeout` → `504`
//!   - その他 → `502`
//!
//! ## クエリパラメータ
//!
//! - `url` (必須): 取得対象の絶対 URL。`http`/`https` のみ受理。
//! - `variant` (任意、既定 `avatar`): `avatar` / `emoji` / `thumbnail` /
//!   `preview` / `header` のいずれか。media-proxy 側の [`Variant`] と同じ表記。
//!
//! ## キャッシュ
//!
//! `Cache-Control: public, max-age=86400, immutable` を載せる。同じ URL は
//! 中身が変わらない (= CDN 上のアバター URL は通常 hash 入り) 前提で、
//! 1 日キャッシュさせて TUI 再描画の負荷を減らす。

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use sakurasato_core::net_guard::host_blocked;
use serde::Deserialize;
use serde_json::json;

use crate::media_proxy_client::MediaProxyError;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct ProxyQuery {
    pub url: String,
    #[serde(default = "default_variant")]
    pub variant: String,
}

fn default_variant() -> String {
    "avatar".to_string()
}

/// media-proxy 側 [`Variant`] にマップできる値だけ通す。
fn validate_variant(v: &str) -> bool {
    matches!(v, "avatar" | "emoji" | "thumbnail" | "preview" | "header")
}

pub async fn handle(State(state): State<AppState>, Query(q): Query<ProxyQuery>) -> Response {
    // 早期検証: URL シンタックスと scheme と SSRF guard を server 側でも通す。
    // media-proxy も同じ検査をするが、ここで弾ければ余分な socket 往復を
    // 節約できる。多層防御。
    let parsed = match url::Url::parse(&q.url) {
        Ok(u) => u,
        Err(e) => return error_400(format!("invalid url: {e}")),
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return error_400(format!("unsupported scheme {:?}", parsed.scheme()));
    }
    if let Some(reason) = host_blocked(&parsed) {
        return error_400(format!(
            "host {:?} blocked: {reason}",
            parsed.host_str().unwrap_or("")
        ));
    }
    if !validate_variant(&q.variant) {
        return error_400(format!("invalid variant {:?}", q.variant));
    }

    match state
        .media_proxy()
        .fetch_image(parsed.as_str(), &q.variant)
        .await
    {
        Ok(processed) => {
            let mut response = (StatusCode::OK, Body::from(processed.bytes)).into_response();
            let headers = response.headers_mut();
            // Content-Type は media-proxy が返した値をそのまま流す (通常 image/webp)。
            if let Ok(ct) = HeaderValue::from_str(&processed.content_type) {
                headers.insert(header::CONTENT_TYPE, ct);
            }
            // 同じ URL は短時間で内容が変わらない前提でキャッシュを促す。
            headers.insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=86400, immutable"),
            );
            headers.insert(
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            );
            response
        }
        Err(err) => map_proxy_error(&err),
    }
}

fn map_proxy_error(err: &MediaProxyError) -> Response {
    match err {
        MediaProxyError::Timeout(_) => error_status(StatusCode::GATEWAY_TIMEOUT, err.to_string()),
        MediaProxyError::Transport(_) | MediaProxyError::MissingContentType => {
            tracing::warn!(?err, "media-proxy transport failure");
            error_status(StatusCode::BAD_GATEWAY, err.to_string())
        }
        MediaProxyError::TooLarge => error_status(StatusCode::PAYLOAD_TOO_LARGE, err.to_string()),
        MediaProxyError::Upstream {
            status,
            reason,
            message,
        } => {
            // media-proxy 側のステータスを基本に転送するが、SSRF 系 (403) は
            // クライアントの責で 400 に丸める ── TUI ユーザに「上流が拒否」
            // ではなく「URL が無効」と見せる方が自然。
            let mapped = match status.as_u16() {
                400 | 403 => StatusCode::BAD_REQUEST,
                413 => StatusCode::PAYLOAD_TOO_LARGE,
                504 => StatusCode::GATEWAY_TIMEOUT,
                // 415 (= upstream が想定外の format を返した) も含めて、
                // それ以外は本サーバから見ると「上流壊れ」なので 502。
                _ => StatusCode::BAD_GATEWAY,
            };
            error_status(mapped, format!("{reason}: {message}"))
        }
    }
}

fn error_400(message: impl Into<String>) -> Response {
    error_status(StatusCode::BAD_REQUEST, message)
}

fn error_status(status: StatusCode, message: impl Into<String>) -> Response {
    let body = axum::Json(json!({"error": message.into()}));
    (status, body).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variant_validation() {
        for v in ["avatar", "emoji", "thumbnail", "preview", "header"] {
            assert!(validate_variant(v), "{v}");
        }
        assert!(!validate_variant(""));
        assert!(!validate_variant("AVATAR"));
        assert!(!validate_variant("banner"));
        assert!(!validate_variant("../etc/passwd"));
    }
}
