//! TUI 絵文字管理画面 (`:emojis`) 向けローカル API。
//!
//! - `POST /api/v1/emojis/import` ── Misskey 形式 zip をアップロードして
//!   取り込む。既存 `sakurasato-server emoji import <zip>` CLI
//!   ([`crate::emoji_import`]) と同一ロジック (`import_archive`) を再利用する。
//! - `GET /api/v1/emojis/remote?q=&limit=` ── DB にキャッシュ済みの remote
//!   emoji (`EmojiReact` 受信で自動学習済み、`host IS NOT NULL`) を検索する。
//!   新規に外部インスタンスへ fetch しには行かない ── 対象は常に既存キャッシュ。
//! - `POST /api/v1/emojis/local/from-remote` ── 検索結果から選んだ 1 件を
//!   ローカル絵文字としてコピーする。versitygw 上の既存オブジェクト
//!   (`emoji/remote/<host>/<shortcode>.webp`) を GET → 同バイト列を
//!   `emoji/local/<shortcode>.webp` に PUT するだけで、画像の再デコード・
//!   再サニタイズも新規の外部 fetch も発生しない (CLAUDE.md §7 の「本体は
//!   デコードしない」を素で満たす)。shortcode は常に元のまま (リネーム無し)
//!   ── 既存ローカルと同名なら [`repo::emoji::upsert_local`] の仕様どおり上書き。

use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::operation::get_object::GetObjectError;
use aws_sdk_s3::primitives::ByteStream;
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use sakurasato_core::repo;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::error;

use crate::local_api::emojis::EmojiItem;
use crate::local_api::media::build_media_url;
use crate::state::AppState;

const DEFAULT_LIMIT: i64 = 20;
/// [`super::emojis::MAX_LIMIT`] と同じ値 (お一人様 + UDS 前提で帯域は無視できる)。
const MAX_LIMIT: i64 = 10000;

/// `POST /api/v1/emojis/import` ── zip 全体はルータ側で
/// `media_proxy.emoji_import.max_zip_bytes` に `DefaultBodyLimit` が掛かる
/// ([`crate::local_api::router`])。ここでは空 body と壊れた zip だけ弾く。
pub async fn import(State(state): State<AppState>, body: Bytes) -> Response {
    if body.is_empty() {
        return bad_request("request body is empty");
    }
    let cursor = std::io::Cursor::new(body);
    let mut archive = match zip::ZipArchive::new(cursor) {
        Ok(a) => a,
        Err(err) => {
            return error_with_body(
                StatusCode::BAD_REQUEST,
                &format!("invalid zip archive: {err}"),
            );
        }
    };
    match crate::emoji_import::import_archive(&state, &mut archive).await {
        Ok(summary) => (StatusCode::OK, Json(summary)).into_response(),
        Err(err) => error_with_body(StatusCode::BAD_REQUEST, &err.to_string()),
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct RemoteListQuery {
    /// 部分一致検索キー (shortcode / host, `ILIKE`)。空/未指定なら全件。
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct RemoteEmojiItem {
    pub id: i64,
    pub shortcode: String,
    pub host: String,
    pub url: String,
    pub media_type: String,
    pub category: Option<String>,
    pub aliases: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct RemoteListResponse {
    pub items: Vec<RemoteEmojiItem>,
}

/// `GET /api/v1/emojis/remote?q=&limit=`
pub async fn search_remote(
    State(state): State<AppState>,
    Query(params): Query<RemoteListQuery>,
) -> Response {
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let q = params.q.as_deref().unwrap_or("").trim();

    let rows = if q.is_empty() {
        repo::emoji::list_remote_cached(state.pool(), limit).await
    } else {
        repo::emoji::search_remote_cached(state.pool(), q, limit).await
    };
    let rows = match rows {
        Ok(rows) => rows,
        Err(err) => {
            error!(?err, "search remote emojis failed");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };

    let host = &state.config().server.host;
    // `list_remote_cached`/`search_remote_cached` は既に `host IS NOT NULL
    // AND image_key IS NOT NULL` で絞っているが、型は `Option` のままなので
    // ここでも防御的に filter_map する。
    let items: Vec<RemoteEmojiItem> = rows
        .into_iter()
        .filter_map(|row| {
            let image_key = row.image_key.as_deref()?;
            let remote_host = row.host.clone()?;
            Some(RemoteEmojiItem {
                id: row.id,
                shortcode: row.shortcode,
                host: remote_host,
                url: build_media_url(host, image_key),
                media_type: row.media_type,
                category: row.category,
                aliases: row.aliases.0,
            })
        })
        .collect();

    (StatusCode::OK, Json(RemoteListResponse { items })).into_response()
}

#[derive(Debug, Deserialize)]
pub struct CopyFromRemoteRequest {
    pub remote_emoji_id: i64,
}

/// `POST /api/v1/emojis/local/from-remote` ── リネーム無しでコピーする
/// (ユーザー合意事項)。既存ローカルと同名なら [`repo::emoji::upsert_local`]
/// の仕様どおり上書きされる。
pub async fn copy_from_remote(
    State(state): State<AppState>,
    Json(req): Json<CopyFromRemoteRequest>,
) -> Response {
    let row = match repo::emoji::get_by_id(state.pool(), req.remote_emoji_id).await {
        Ok(Some(r)) => r,
        Ok(None) => return error_with_body(StatusCode::NOT_FOUND, "emoji not found"),
        Err(err) => {
            error!(?err, "get_by_id failed");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    if row.host.is_none() {
        return bad_request("emoji is not a remote emoji");
    }
    let Some(image_key) = row.image_key.clone() else {
        return error_with_body(
            StatusCode::CONFLICT,
            "remote emoji has no cached image (fetch previously failed)",
        );
    };
    // DB の CHECK 制約を既に通過済みの値だが、versitygw キーの一部として
    // 使うため念のため再検証する (zip-slip と同種の多層防御、emoji_import.rs
    // と同じ考え方)。
    if !repo::emoji::is_valid_shortcode(&row.shortcode) {
        error!(shortcode = %row.shortcode, "remote emoji shortcode failed re-validation");
        return error_with_body(StatusCode::UNPROCESSABLE_ENTITY, "shortcode is invalid");
    }

    let bucket = state.config().storage.bucket.clone();
    let get_result = state
        .s3_client()
        .get_object()
        .bucket(&bucket)
        .key(&image_key)
        .send()
        .await;
    let resp = match get_result {
        Ok(r) => r,
        Err(SdkError::ServiceError(svc)) if matches!(svc.err(), GetObjectError::NoSuchKey(_)) => {
            error!(
                image_key,
                "remote emoji image_key missing in versitygw (DB/store drift)"
            );
            return error_with_body(StatusCode::BAD_GATEWAY, "cached image object missing");
        }
        Err(err) => {
            error!(?err, image_key, "get_object failed");
            return error_with_body(StatusCode::BAD_GATEWAY, "object store read failed");
        }
    };
    let media_type = resp
        .content_type()
        .map_or_else(|| row.media_type.clone(), ToString::to_string);
    let bytes = match resp.body.collect().await {
        Ok(agg) => agg.into_bytes(),
        Err(err) => {
            error!(?err, image_key, "collect body failed");
            return error_with_body(StatusCode::BAD_GATEWAY, "object store read failed");
        }
    };

    let new_key = format!("emoji/local/{}.webp", row.shortcode);
    let put_result = state
        .s3_client()
        .put_object()
        .bucket(&bucket)
        .key(&new_key)
        .content_type(&media_type)
        .body(ByteStream::from(bytes))
        .send()
        .await;
    if let Err(err) = put_result {
        error!(?err, new_key, "put_object failed");
        return error_with_body(StatusCode::BAD_GATEWAY, "object store write failed");
    }

    let new = repo::emoji::NewLocalEmoji {
        shortcode: row.shortcode.clone(),
        category: row.category.clone(),
        aliases: row.aliases.0.clone(),
        image_key: new_key.clone(),
        media_type: media_type.clone(),
        license: row.license.clone(),
        is_sensitive: row.is_sensitive,
    };
    match repo::emoji::upsert_local(state.pool(), new).await {
        Ok(_) => {
            let host = &state.config().server.host;
            let item = EmojiItem {
                kind: "custom",
                url: build_media_url(host, &new_key),
                shortcode: row.shortcode,
                media_type,
                category: row.category,
                aliases: row.aliases.0,
            };
            (StatusCode::OK, Json(item)).into_response()
        }
        Err(err) => {
            error!(?err, "upsert_local failed");
            error_with_body(StatusCode::SERVICE_UNAVAILABLE, "database update failed")
        }
    }
}

fn bad_request(reason: &'static str) -> Response {
    error_with_body(StatusCode::BAD_REQUEST, reason)
}

fn error_with_body(status: StatusCode, reason: &str) -> Response {
    (status, Json(json!({ "error": reason }))).into_response()
}
