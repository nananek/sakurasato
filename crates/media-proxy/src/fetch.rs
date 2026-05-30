//! `POST /v1/image/fetch` — リモート URL を取得し、画像として再エンコードして返す。
//!
//! # 期待リクエスト
//!
//! ```json
//! { "url": "https://cdn.example/avatar.png", "variant": "avatar" }
//! ```
//!
//! # 期待レスポンス
//!
//! - 成功: `200 OK`、`Content-Type: image/webp`、本文は変換後のバイト列。
//!   - `X-Source-Url`: 元 URL (= server 側のログに紐付けるため)
//!   - `X-Output-Width` / `X-Output-Height`: 変換後寸法
//! - 失敗: `4xx`/`5xx`、`Content-Type: application/json`、本文は
//!   `{ "error": "...", "reason": "..." }`。

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::HeaderValue;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::StreamExt;
use sakurasato_core::net_guard::host_blocked;
use serde::Deserialize;
use url::Url;

use crate::error::ApiError;
use crate::image_pipeline::{self, Variant};
use crate::state::ProxyState;

#[derive(Debug, Deserialize)]
pub struct FetchRequest {
    pub url: String,
    pub variant: Variant,
}

pub async fn handle(
    State(state): State<Arc<ProxyState>>,
    Json(req): Json<FetchRequest>,
) -> Response {
    match fetch_inner(&state, &req).await {
        Ok(resp) => resp,
        Err(err) => err.into_response(),
    }
}

async fn fetch_inner(state: &ProxyState, req: &FetchRequest) -> Result<Response, ApiError> {
    let url = Url::parse(&req.url)
        .map_err(|e| ApiError::bad_request("invalid_url", format!("parse url: {e}")))?;

    if !matches!(url.scheme(), "http" | "https") {
        return Err(ApiError::bad_request(
            "unsupported_scheme",
            format!("scheme {:?} is not http/https", url.scheme()),
        ));
    }

    if let Some(reason) = host_blocked(&url) {
        return Err(ApiError::blocked(
            reason,
            format!(
                "host {:?} is blocked: {reason}",
                url.host_str().unwrap_or("")
            ),
        ));
    }

    let bytes = download(state, &url).await?;
    let processed = image_pipeline::process(&bytes, req.variant, state.max_pixels())?;

    let mut response = (axum::http::StatusCode::OK, processed.bytes).into_response();
    let headers = response.headers_mut();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static(processed.content_type),
    );
    if let Ok(src) = HeaderValue::from_str(req.url.as_str()) {
        headers.insert("x-source-url", src);
    }
    if let Ok(v) = HeaderValue::from_str(&processed.width.to_string()) {
        headers.insert("x-output-width", v);
    }
    if let Ok(v) = HeaderValue::from_str(&processed.height.to_string()) {
        headers.insert("x-output-height", v);
    }
    Ok(response)
}

/// `url` に GET を投げ、本文を `max_bytes` で頭打ちにしながら集める。
///
/// `Content-Length` が `max_bytes` 超過を申告した時点で即拒否し、累計でも
/// 検査する (chunked / gzip でヘッダが嘘でも防げる)。
async fn download(state: &ProxyState, url: &Url) -> Result<Bytes, ApiError> {
    let max_bytes = state.max_bytes();
    let resp = state
        .http()
        .get(url.clone())
        .header(
            "accept",
            "image/png, image/jpeg, image/webp, image/gif;q=0.5, image/*;q=0.1",
        )
        .send()
        .await
        .map_err(|err| classify_reqwest_err("send", &err))?;

    if !resp.status().is_success() {
        return Err(ApiError::upstream(
            "upstream_http",
            format!("upstream returned {}", resp.status().as_u16()),
        ));
    }

    if let Some(len) = resp.content_length()
        && len > state.config().media_proxy.max_bytes
    {
        return Err(ApiError::too_large(format!(
            "Content-Length {len} exceeds max_bytes {}",
            state.config().media_proxy.max_bytes
        )));
    }

    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|err| classify_reqwest_err("read_body", &err))?;
        if buf.len().saturating_add(chunk.len()) > max_bytes {
            return Err(ApiError::too_large(format!(
                "body exceeds max_bytes {max_bytes} during stream",
            )));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(buf))
}

/// `reqwest::Error` を [`ApiError`] にマップする。redirect SSRF / timeout /
/// transport を切り分けて呼び出し側 (server) が分類できる `reason` に落とす。
fn classify_reqwest_err(phase: &'static str, err: &reqwest::Error) -> ApiError {
    if err.is_timeout() {
        return ApiError::timeout(format!("{phase}: {err}"));
    }
    if err.is_redirect() {
        // redirect policy が `attempt.error(..)` で止めた場合 ── ブロック先や
        // hop 過多。reason は `redirect_blocked` で集計しやすくする。
        return ApiError::blocked("redirect_blocked", format!("{phase}: {err}"));
    }
    if err.is_connect() {
        return ApiError::upstream("connect", format!("{phase}: {err}"));
    }
    ApiError::upstream("transport", format!("{phase}: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetch_request_deserializes_variant() {
        let req: FetchRequest =
            serde_json::from_str(r#"{"url":"https://x/y","variant":"avatar"}"#).unwrap();
        assert_eq!(req.url, "https://x/y");
        assert_eq!(req.variant, Variant::Avatar);
    }
}
