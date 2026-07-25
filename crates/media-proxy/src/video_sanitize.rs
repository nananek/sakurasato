//! `POST /v1/video/sanitize` — アップロードされた動画バイト列のコンテナ
//! メタデータを無害化する。
//!
//! `sanitize.rs` (画像) と対の薄いハンドラ。`variant` クエリは無い (動画は
//! リサイズしない ── サイズバリアントという概念自体が無い)。
//!
//! # 入出力
//!
//! - 入力: raw バイト列を本文に載せる (`video/mp4` または `video/webm`)。
//! - 出力: 入力と同じ `Content-Type` (再エンコードしないため)。
//!   `x-output-width` / `x-output-height` / `x-output-duration-ms` ヘッダに
//!   コンテナヘッダから読み取った寸法・再生時間を載せる。

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::error::ApiError;
use crate::state::ProxyState;
use crate::video_pipeline;

pub async fn handle(State(state): State<Arc<ProxyState>>, body: Bytes) -> Response {
    match sanitize_inner(&state, &body) {
        Ok(resp) => resp,
        Err(err) => err.into_response(),
    }
}

fn sanitize_inner(state: &ProxyState, body: &[u8]) -> Result<Response, ApiError> {
    if body.len() > state.max_video_bytes() {
        return Err(ApiError::too_large(format!(
            "upload {} exceeds max_bytes {}",
            body.len(),
            state.max_video_bytes()
        )));
    }

    let processed = video_pipeline::process(body, state.max_video_duration_ms())?;

    let mut response = (StatusCode::OK, processed.bytes).into_response();
    let headers = response.headers_mut();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static(processed.content_type),
    );
    if let Ok(v) = HeaderValue::from_str(&processed.width.to_string()) {
        headers.insert("x-output-width", v);
    }
    if let Ok(v) = HeaderValue::from_str(&processed.height.to_string()) {
        headers.insert("x-output-height", v);
    }
    if let Ok(v) = HeaderValue::from_str(&processed.duration_ms.to_string()) {
        headers.insert("x-output-duration-ms", v);
    }
    Ok(response)
}
