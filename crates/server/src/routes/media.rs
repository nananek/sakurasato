//! `GET /media/{*key}` — versitygw に格納された画像を配信する。
//!
//! versitygw は CLAUDE.md §3 で「非公開・内部ネット限定」と定めているため、
//! 公開クライアント (連合 / ブラウザ) は本ルート経由でのみオブジェクトを
//! 取得できる。署名 URL や ACL は使わず、本サーバが代理で GET する。
//!
//! ## URL 設計
//!
//! axum 0.8 のワイルドカード `{*key}` を使い、`/media/path/to/object.png`
//! のようなスラッシュ入りキーを単一の `String` でキャプチャする。
//!
//! ## 本文の扱い
//!
//! M4 PR1 では **`GetObject` のレスポンスボディを一括バッファリング** して
//! `Body::from(bytes)` で返す。max オブジェクトサイズは `media_proxy.max_bytes`
//! (既定 25 MiB) で、アップロードサニタイザ側で上限を強制している前提。
//! 本来はストリーミング (`Body::from_stream` + `ByteStream`) が望ましいが、
//! `ByteStream` の `Stream` 実装を axum の `Body::from_stream` シグネチャに
//! 適合させる際に型のマッサージが要るため、PR1 ではシンプルに collect する。
//! 巨大オブジェクトを扱うようになった段階で stream 化する (今後の milestone)。
//!
//! ## エラー
//!
//! - `NoSuchKey` (オブジェクト不在) → 404。
//! - その他の S3 エラー → 500 (詳細はログ出力のみ、本文に出さない)。

use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::operation::get_object::GetObjectError;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::state::AppState;

pub async fn handle(State(state): State<AppState>, Path(key): Path<String>) -> Response {
    let bucket = state.config().storage.bucket.clone();
    let resp = match state
        .s3_client()
        .get_object()
        .bucket(bucket)
        .key(&key)
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(SdkError::ServiceError(svc)) if matches!(svc.err(), GetObjectError::NoSuchKey(_)) => {
            return StatusCode::NOT_FOUND.into_response();
        }
        Err(err) => {
            tracing::error!(?err, key = %key, "media GET failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let content_type_header = resp
        .content_type()
        .and_then(|s| HeaderValue::from_str(s).ok())
        .unwrap_or_else(|| HeaderValue::from_static("application/octet-stream"));
    let content_length = resp.content_length();

    // body は ByteStream。`collect()` で全部バッファリングし `Body::from`。
    let bytes = match resp.body.collect().await {
        Ok(agg) => agg.into_bytes(),
        Err(err) => {
            tracing::error!(?err, key = %key, "media GET: collect body failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let mut response = (StatusCode::OK, Body::from(bytes)).into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, content_type_header);
    if let Some(len) = content_length
        && let Ok(hv) = HeaderValue::from_str(&len.to_string())
    {
        response.headers_mut().insert(header::CONTENT_LENGTH, hv);
    }
    response
}
