//! `POST /v1/image/sanitize` — アップロードされた生バイト列を再エンコードする。
//!
//! `M7` (TUI からのアイコン / ヘッダ / 添付アップロード) で使う想定。M6 では
//! 「外部から来た / ユーザが入れたバイト列を server 本体にデコードさせない」
//! 原則を担保するために先に実装し、ハンドラ単独で叩けるようにしておく。
//!
//! # 入出力
//!
//! - 入力: raw バイト列を本文に載せる (`Content-Type` ヒントは任意)。
//! - クエリ: `?variant=avatar|preview|thumbnail|header`
//! - 出力: `image/webp`、寸法は変換後。

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::error::ApiError;
use crate::image_pipeline::{self, Variant};
use crate::state::ProxyState;

#[derive(Debug, Deserialize)]
pub struct SanitizeQuery {
    pub variant: Variant,
}

pub async fn handle(
    State(state): State<Arc<ProxyState>>,
    Query(q): Query<SanitizeQuery>,
    body: Bytes,
) -> Response {
    match sanitize_inner(&state, q.variant, &body) {
        Ok(resp) => resp,
        Err(err) => err.into_response(),
    }
}

fn sanitize_inner(state: &ProxyState, variant: Variant, body: &[u8]) -> Result<Response, ApiError> {
    if body.len() > state.max_bytes() {
        return Err(ApiError::too_large(format!(
            "upload {} exceeds max_bytes {}",
            body.len(),
            state.max_bytes()
        )));
    }

    let processed = image_pipeline::process(body, variant, state.max_pixels())?;

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
    Ok(response)
}
