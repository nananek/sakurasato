//! `url`/`variant` クエリを media-proxy に橋渡しする HTTP ハンドラの共通コア。
//!
//! 2 箇所から使われる:
//!
//! - `GET /api/v1/media/proxy` ([`crate::local_api::media_proxy`]) ── TUI 用。
//!   `/api/v1/*` 全体にかかる Bearer 認証の内側。
//! - `GET /media-proxy` ([`crate::routes::media_proxy`]) ── `MiAuth` 経路
//!   ([`crate::miauth::conv`]) が返す `avatarUrl` / `bannerUrl` / 添付 `url` /
//!   `thumbnailUrl` / 本文 `:emoji:` 画像の実体。Misskey 互換クライアント
//!   (Aria 等) の画像ウィジェットは任意ヘッダを付けずに直接この URL を GET
//!   するため、**無認証** かつ **`/media/<key>` と同じ公開 AP host
//!   (`config.server.host`) 上** に置く必要がある (= `MiAuth` 専用 socket は
//!   Tailscale tailnet 越しでしか到達できない構成が推奨で、host が食い違う
//!   と画像が読み込めない。`MiAuth` の全 handler が emoji/添付 URL を一貫して
//!   `config.server.host` で組み立てている既存慣習 (`local_api::media::build_media_url`)
//!   に合わせる)。
//!
//! 無認証で公開する以上「任意 URL を無制限に fetch できる open relay」化を
//! 避ける必要がある。SSRF ガード ([`sakurasato_core::net_guard::host_blocked`])
//! に加え、呼び出し側 ([`crate::routes::media_proxy`]) が宛先 host ごとの
//! per-domain レート制限を掛ける ── [`crate::state::AppState::try_acquire_fetch`]
//! (AP object fetch 用) とは **別バケット** を使い、この公開エンドポイントへの
//! flood が正規の AP fetch 用トークンを枯渇させないようにする。
//!
//! ## レスポンス
//!
//! - 成功: `200 OK`、`Content-Type` は media-proxy が返した値 (`image/webp`)、
//!   本文は変換後のバイト列。`Cache-Control: public, max-age=86400,
//!   immutable` を付ける (= 同じ URL は短時間で内容が変わらない前提)。
//! - 失敗: media-proxy の `reason` を見て分類 ([`map_proxy_error`]):
//!   - `invalid_url` / `unsupported_scheme` / `empty_body` → `400`
//!   - `loopback` / `private` / 等 SSRF 系 → `400` (= クライアントの責)
//!   - `too_large` → `413`
//!   - `unsupported_format` / `decode_failed` 等 → `502` (= 上流が壊れている)
//!   - `upstream_http` / `transport` / `connect` → `502`
//!   - `upstream_timeout` → `504`
//!   - その他 → `502`
//!
//! `url`/`variant` の scheme / SSRF / variant 検証は [`validate`] が本サーバ側
//! でも先に行う (media-proxy も同じ検査をするが、ここで弾ければ UDS 往復を
//! 1 回節約できる多層防御)。
//!
//! ## クエリパラメータ
//!
//! - `url` (必須): 取得対象の絶対 URL。`http`/`https` のみ受理。
//! - `variant` (任意、既定 `avatar`): `avatar` / `emoji` / `thumbnail` /
//!   `preview` / `header` のいずれか (media-proxy 側の `Variant` と同じ表記)。

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
pub(crate) fn validate_variant(v: &str) -> bool {
    matches!(v, "avatar" | "emoji" | "thumbnail" | "preview" | "header")
}

/// クエリを検証し、パース済み URL を返す。シンタックス / scheme / SSRF /
/// variant のいずれかで弾かれたら `Err(400 Response)`。
pub(crate) fn validate(q: &ProxyQuery, allow_private: bool) -> Result<url::Url, Response> {
    let parsed = url::Url::parse(&q.url).map_err(|e| error_400(format!("invalid url: {e}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(error_400(format!(
            "unsupported scheme {:?}",
            parsed.scheme()
        )));
    }
    if !allow_private && let Some(reason) = host_blocked(&parsed) {
        return Err(error_400(format!(
            "host {:?} blocked: {reason}",
            parsed.host_str().unwrap_or("")
        )));
    }
    if !validate_variant(&q.variant) {
        return Err(error_400(format!("invalid variant {:?}", q.variant)));
    }
    Ok(parsed)
}

/// 検証済み URL を media-proxy 経由で取得し、レスポンスに変換する。
pub(crate) async fn fetch_and_respond(state: &AppState, url: &str, variant: &str) -> Response {
    match state.media_proxy().fetch_image(url, variant).await {
        Ok(processed) => {
            let mut response =
                (StatusCode::OK, axum::body::Body::from(processed.bytes)).into_response();
            let headers = response.headers_mut();
            if let Ok(ct) = HeaderValue::from_str(&processed.content_type) {
                headers.insert(header::CONTENT_TYPE, ct);
            }
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
            // クライアントの責で 400 に丸める ── 「上流が拒否」ではなく
            // 「URL が無効」と見せる方が自然。
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

pub(crate) fn error_400(message: impl Into<String>) -> Response {
    error_status(StatusCode::BAD_REQUEST, message)
}

pub(crate) fn error_status(status: StatusCode, message: impl Into<String>) -> Response {
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

    #[test]
    fn private_host_requires_explicit_test_opt_in() {
        let query = ProxyQuery {
            url: "http://media-host/avatar.png".into(),
            variant: "avatar".into(),
        };
        assert!(validate(&query, false).is_err());
        assert!(validate(&query, true).is_ok());
    }
}
