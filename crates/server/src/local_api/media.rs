//! `POST /api/v1/media` — TUI からの画像アップロード (M7)。
//!
//! 流れ (CLAUDE.md §5.2 / §5.3):
//!
//! 1. ローカル actor (`config.server.user`) を解決する。`init` 未実行は 503。
//! 2. クエリ `kind` (avatar / header / attachment) を検証。`alt` (代替テキスト)
//!    は任意で、長さ上限を `ALT_MAX` で切る。
//! 3. リクエスト本文をバイト列で受け取る (= TUI は raw `application/octet-stream`
//!    か `image/<fmt>` で送る)。axum の `Bytes` extractor が `DefaultBodyLimit`
//!    を尊重する。サーバ全体の上限は `media_proxy.max_bytes` を採用する。
//! 4. media-proxy `/v1/image/sanitize?variant=<v>` を呼び、再エンコード後の
//!    WebP バイト列を受け取る。本体 server は中身を **デコードしない**
//!    (= CLAUDE.md §7 の信頼境界を維持)。
//! 5. SHA-256 hex を取り、`storage_key = "<hash>.webp"` を決める。
//!    既存行があれば dedupe して既存の id を返す ── 同じバイト列を何度
//!    上げても storage と DB はそれぞれ 1 つ。public URL の `/media/`
//!    プレフィックスは [`build_media_url`] が付ける。
//! 6. versitygw に PUT object する。失敗時は 502 (DB は触らない)。
//! 7. `media` 行を insert し、JSON で返す。
//!
//! ## URL 公開
//!
//! 返却する `url` は `https://<host>/media/<key>` の絶対 URL。連合相手から
//! 参照される actor `icon` / `image` / note `attachment` の URL として
//! そのまま出る。versitygw 自体は非公開 (CLAUDE.md §3) で、本サーバの
//! `routes::media::handle` が代理 GET する設計。
//!
//! ## 認証
//!
//! `/api/v1/*` 全体にかかる Bearer 認証 ([`super::auth::require_token`]) に
//! よって本ハンドラに到達する時点で「ローカル UID 同じ・有効トークン保持」
//! が確定している。お一人様サーバのため、ハンドラ内で actor 紐付けを
//! `config.server.user` から決め打ちしている (multi-user 化したらここを変える)。

use aws_sdk_s3::primitives::ByteStream;
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base16ct::lower as base16;
use sakurasato_core::model::{ActorRow, MediaRow};
use sakurasato_core::repo;
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use tracing::{error, warn};

use crate::media_proxy_client::MediaProxyError;
use crate::sign::digest::sha256;
use crate::state::AppState;

/// 受け入れる `kind` の上限。
const KIND_AVATAR: &str = "avatar";
const KIND_HEADER: &str = "header";
const KIND_ATTACHMENT: &str = "attachment";

/// `alt_text` の文字数上限 (Mastodon は 1500、Misskey は 512)。
/// 安全側で 1500 にしておく ── 連合先でクリップされる可能性はあるが、
/// 短く切るより長く受けてサーバが落ちない方が大事。
const ALT_MAX: usize = 1500;

#[derive(Debug, Deserialize)]
pub struct UploadQuery {
    pub kind: String,
    #[serde(default)]
    pub alt: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct MediaResponse {
    pub id: i64,
    pub storage_key: String,
    pub url: String,
    pub media_type: String,
    pub width: i32,
    pub height: i32,
    pub byte_size: i64,
    pub kind: String,
    pub alt_text: Option<String>,
    /// 動画の再生時間 (ミリ秒)。画像は常に `None`。
    pub duration_ms: Option<i64>,
}

impl MediaResponse {
    pub(crate) fn from_row(row: &MediaRow, host: &str) -> Self {
        Self {
            id: row.id,
            storage_key: row.storage_key.clone(),
            url: build_media_url(host, &row.storage_key),
            media_type: row.media_type.clone(),
            width: row.width,
            height: row.height,
            byte_size: row.byte_size,
            kind: row.kind.clone(),
            alt_text: row.alt_text.clone(),
            duration_ms: row.duration_ms,
        }
    }
}

/// `https://<host>/media/<storage_key>` を組み立てる。`storage_key` は
/// 我々が生成する ASCII (`<hex>.webp`) で encode 不要。
pub(crate) fn build_media_url(host: &str, storage_key: &str) -> String {
    format!("https://{host}/media/{storage_key}")
}

/// 1 件の `media` 行を AP の `Document` JSON にする。
///
/// AS2 `Document` で `mediaType` + `url` + `name` (alt) を載せる。`width` /
/// `height` は Mastodon 拡張だが幅広く受け入れられている (Misskey も読む)。
///
/// 配置: 元々 `notes.rs` の private fn だったが、`permalink.rs` で Note の AP
/// JSON 再 fetch にも `attachment` を出すために共通化して `media.rs` に移した
/// (= `build_media_url` と同居する方が依存関係が綺麗)。
pub(crate) fn attachment_document(host: &str, m: &MediaRow) -> JsonValue {
    let mut obj = json!({
        "type": "Document",
        "mediaType": m.media_type,
        "url": build_media_url(host, &m.storage_key),
        "width": m.width,
        "height": m.height,
    });
    if let Some(alt) = m.alt_text.as_ref()
        && !alt.is_empty()
    {
        obj["name"] = JsonValue::String(alt.clone());
    }
    obj
}

/// `kind` を media-proxy の variant 文字列にマップする。
/// `attachment` は 1280x1280 ボックスを使うので `preview` に展開する。
fn variant_for(kind: &str) -> Option<&'static str> {
    match kind {
        KIND_AVATAR => Some("avatar"),
        KIND_HEADER => Some("header"),
        KIND_ATTACHMENT => Some("preview"),
        _ => None,
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "upload pipeline is mostly straight-line"
)]
pub async fn upload(
    State(state): State<AppState>,
    Query(q): Query<UploadQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    match upload_media_core(&state, &q.kind, q.alt.as_deref(), content_type, body).await {
        Ok((row, created)) => {
            // dedupe ヒット (= 同一バイト列の既存行) は 200、新規 INSERT は 201。
            let status = if created {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            (status, Json(MediaResponse::from_row(&row, host_of(&state)))).into_response()
        }
        Err(resp) => resp,
    }
}

/// アップロードの共通入り口。`Content-Type` ヒント (`video/*` かどうか) で
/// 画像経路 ([`upload_image_core`]) / 動画経路 ([`upload_video_core`]) に
/// 分岐する。**このヒントはクライアント申告に過ぎず権威ではない** ──
/// 実際のフォーマット検証は media-proxy の box/element walker
/// ([`sakurasato_media_proxy::video_pipeline`]) が構造的に行う (= 画像側の
/// `variant` クエリと同じ考え方)。
pub(crate) async fn upload_media_core(
    state: &AppState,
    kind: &str,
    alt: Option<&str>,
    content_type: Option<&str>,
    body: Bytes,
) -> Result<(MediaRow, bool), Response> {
    if let Some(alt) = alt
        && alt.chars().count() > ALT_MAX
    {
        return Err(bad_request("alt text exceeds the 1500-character limit"));
    }
    if body.is_empty() {
        return Err(bad_request("request body is empty"));
    }

    let is_video_hint = content_type.is_some_and(|ct| ct.starts_with("video/"));
    if is_video_hint {
        upload_video_core(state, kind, alt, body).await
    } else {
        upload_image_core(state, kind, alt, body).await
    }
}

/// 画像アップロードの共通コア。raw body (`local_api`) / multipart (`MiAuth` drive)
/// のどちらの face からも呼べるよう、入力を **再エンコード前バイト列** +
/// `kind` + `alt` に統一する。media-proxy サニタイズ → R2 PUT → `media` 行
/// INSERT まで行い `(行, 新規作成か)` を返す。`created = false` は dedupe ヒット
/// (= 同一 `storage_key` の既存行を再利用)。エラーは face 非依存の `Response`。
#[allow(
    clippy::too_many_lines,
    reason = "validate → media-proxy → dedupe → R2 PUT → INSERT を 1 本のパイプラインに収める"
)]
async fn upload_image_core(
    state: &AppState,
    kind: &str,
    alt: Option<&str>,
    body: Bytes,
) -> Result<(MediaRow, bool), Response> {
    let Some(variant) = variant_for(kind) else {
        return Err(bad_request("kind must be one of avatar/header/attachment"));
    };
    let max_bytes = usize::try_from(state.config().media_proxy.max_bytes).unwrap_or(usize::MAX);
    if body.len() > max_bytes {
        return Err(error_with_body(
            StatusCode::PAYLOAD_TOO_LARGE,
            "upload exceeds media_proxy.max_bytes",
        ));
    }

    let local_actor = resolve_local_actor(state).await?;

    // 1. media-proxy で再エンコード → 安全化済みバイト列
    let processed = match state.media_proxy().sanitize_image(body, variant).await {
        Ok(p) => p,
        Err(err) => return Err(map_proxy_error(&err)),
    };
    let Some(width) = processed.width else {
        error!("media-proxy did not return x-output-width header");
        return Err(error_with_body(
            StatusCode::BAD_GATEWAY,
            "media-proxy missing width",
        ));
    };
    let Some(height) = processed.height else {
        error!("media-proxy did not return x-output-height header");
        return Err(error_with_body(
            StatusCode::BAD_GATEWAY,
            "media-proxy missing height",
        ));
    };

    // 2. SHA-256 → storage_key (= versitygw bucket key)
    //    `routes::media::handle` が `GET /media/{*key}` でこの key を
    //    そのまま versitygw に投げる ── DB の `storage_key` と S3 オブジェクト
    //    key を完全一致させる契約。URL 側の `/media/` プレフィックスは
    //    public URL の path 部分にだけ付ける ([`build_media_url`])。
    let hash = sha256(&processed.bytes);
    let mut hex_buf = [0u8; 64];
    let storage_key = format!(
        "{hash}.webp",
        hash = base16::encode_str(&hash, &mut hex_buf)
            .expect("64-byte buf is enough for 32-byte hash"),
    );

    // 3. dedupe — 既存行があり所有者が同じならそのまま返す。
    //    同じバイト列で別所有者が来ることはお一人様サーバではあり得ないが、
    //    将来の multi-user 化に備えて明示的に分岐する。
    match repo::media::get_by_storage_key(state.pool(), &storage_key).await {
        Ok(Some(existing)) => {
            if existing.owner_actor_id != local_actor.id {
                warn!(
                    storage_key,
                    existing_owner = existing.owner_actor_id,
                    requested_owner = local_actor.id,
                    "media dedupe collision: existing row belongs to another actor"
                );
                return Err(error_with_body(
                    StatusCode::CONFLICT,
                    "media with the same content already exists for another actor",
                ));
            }
            // kind / alt が違っても同じ storage_key (= 同じバイト列) なので
            // 既存行を再利用する。`kind` を上書きしない方針: アバター用に
            // 上げたものを「同じバイト列」として添付目的で再アップしても、
            // DB 上は最初に登録した kind のまま。url だけ返ればよい。
            return Ok((existing, false));
        }
        Ok(None) => {}
        Err(err) => {
            error!(?err, "media dedupe lookup failed");
            return Err(StatusCode::SERVICE_UNAVAILABLE.into_response());
        }
    }

    // 4. versitygw に PUT
    let bucket = state.config().storage.bucket.clone();
    let put_bytes = processed.bytes.clone();
    let byte_size = i64::try_from(put_bytes.len()).unwrap_or(i64::MAX);
    let media_type = processed.content_type.clone();
    let put_result = state
        .s3_client()
        .put_object()
        .bucket(&bucket)
        .key(&storage_key)
        .body(ByteStream::from(put_bytes.to_vec()))
        .content_type(&media_type)
        .send()
        .await;
    if let Err(err) = put_result {
        error!(?err, storage_key, "versitygw PUT failed");
        return Err(error_with_body(
            StatusCode::BAD_GATEWAY,
            "object store write failed",
        ));
    }

    // 5. media 行 insert
    let new = repo::media::NewMedia {
        storage_key: storage_key.clone(),
        media_type,
        width: i32::try_from(width).unwrap_or(i32::MAX),
        height: i32::try_from(height).unwrap_or(i32::MAX),
        byte_size,
        kind: kind.to_string(),
        alt_text: alt.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
        owner_actor_id: local_actor.id,
        duration_ms: None,
    };
    let inserted = match repo::media::insert(state.pool(), new).await {
        Ok(row) => row,
        Err(err) => {
            error!(?err, "media insert failed");
            return Err(StatusCode::SERVICE_UNAVAILABLE.into_response());
        }
    };

    Ok((inserted, true))
}

/// 動画アップロードの共通コア。`kind=attachment` のみ許可 (avatar/header は
/// 画像専用のまま)。media-proxy `/v1/video/sanitize` でコンテナメタデータを
/// 無害化したバイト列を受け取り、R2 PUT → `media` 行 INSERT まで行う。
/// 画像経路と異なり **再エンコードしない** ため、`storage_key` の拡張子は
/// 入力コンテナに応じて `.mp4` / `.webm` を使う。
#[allow(
    clippy::too_many_lines,
    reason = "validate → media-proxy → dedupe → R2 PUT → INSERT を 1 本のパイプラインに収める (upload_image_core と同じ理由)"
)]
async fn upload_video_core(
    state: &AppState,
    kind: &str,
    alt: Option<&str>,
    body: Bytes,
) -> Result<(MediaRow, bool), Response> {
    if kind != KIND_ATTACHMENT {
        return Err(bad_request(
            "video attachments are only allowed for kind=attachment",
        ));
    }
    let max_bytes = state.config().media_proxy.video.max_bytes;
    let max_bytes = usize::try_from(max_bytes).unwrap_or(usize::MAX);
    if body.len() > max_bytes {
        return Err(error_with_body(
            StatusCode::PAYLOAD_TOO_LARGE,
            "upload exceeds media_proxy.video.max_bytes",
        ));
    }

    let local_actor = resolve_local_actor(state).await?;

    // 1. media-proxy でコンテナメタデータ無害化 (再エンコードはしない)
    let processed = match state.media_proxy().sanitize_video(body).await {
        Ok(p) => p,
        Err(err) => return Err(map_proxy_error(&err)),
    };
    let Some(width) = processed.width else {
        error!("media-proxy did not return x-output-width header");
        return Err(error_with_body(
            StatusCode::BAD_GATEWAY,
            "media-proxy missing width",
        ));
    };
    let Some(height) = processed.height else {
        error!("media-proxy did not return x-output-height header");
        return Err(error_with_body(
            StatusCode::BAD_GATEWAY,
            "media-proxy missing height",
        ));
    };
    let Some(duration_ms) = processed.duration_ms else {
        error!("media-proxy did not return x-output-duration-ms header");
        return Err(error_with_body(
            StatusCode::BAD_GATEWAY,
            "media-proxy missing duration",
        ));
    };
    let Some(ext) = video_extension_for(&processed.content_type) else {
        error!(
            content_type = processed.content_type,
            "media-proxy returned unexpected video content-type"
        );
        return Err(error_with_body(
            StatusCode::BAD_GATEWAY,
            "media-proxy returned unsupported video content-type",
        ));
    };

    // 2. SHA-256 → storage_key
    let hash = sha256(&processed.bytes);
    let mut hex_buf = [0u8; 64];
    let storage_key = format!(
        "{hash}.{ext}",
        hash = base16::encode_str(&hash, &mut hex_buf)
            .expect("64-byte buf is enough for 32-byte hash"),
    );

    // 3. dedupe (画像経路と同じ方針)
    match repo::media::get_by_storage_key(state.pool(), &storage_key).await {
        Ok(Some(existing)) => {
            if existing.owner_actor_id != local_actor.id {
                warn!(
                    storage_key,
                    existing_owner = existing.owner_actor_id,
                    requested_owner = local_actor.id,
                    "media dedupe collision: existing row belongs to another actor"
                );
                return Err(error_with_body(
                    StatusCode::CONFLICT,
                    "media with the same content already exists for another actor",
                ));
            }
            return Ok((existing, false));
        }
        Ok(None) => {}
        Err(err) => {
            error!(?err, "media dedupe lookup failed");
            return Err(StatusCode::SERVICE_UNAVAILABLE.into_response());
        }
    }

    // 4. versitygw に PUT
    let bucket = state.config().storage.bucket.clone();
    let put_bytes = processed.bytes.clone();
    let byte_size = i64::try_from(put_bytes.len()).unwrap_or(i64::MAX);
    let media_type = processed.content_type.clone();
    let put_result = state
        .s3_client()
        .put_object()
        .bucket(&bucket)
        .key(&storage_key)
        .body(ByteStream::from(put_bytes.to_vec()))
        .content_type(&media_type)
        .send()
        .await;
    if let Err(err) = put_result {
        error!(?err, storage_key, "versitygw PUT failed");
        return Err(error_with_body(
            StatusCode::BAD_GATEWAY,
            "object store write failed",
        ));
    }

    // 5. media 行 insert
    let new = repo::media::NewMedia {
        storage_key: storage_key.clone(),
        media_type,
        width: i32::try_from(width).unwrap_or(i32::MAX),
        height: i32::try_from(height).unwrap_or(i32::MAX),
        byte_size,
        kind: kind.to_string(),
        alt_text: alt.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
        owner_actor_id: local_actor.id,
        duration_ms: i64::try_from(duration_ms).ok(),
    };
    let inserted = match repo::media::insert(state.pool(), new).await {
        Ok(row) => row,
        Err(err) => {
            error!(?err, "media insert failed");
            return Err(StatusCode::SERVICE_UNAVAILABLE.into_response());
        }
    };

    Ok((inserted, true))
}

/// media-proxy が返した `Content-Type` から storage key の拡張子を決める。
fn video_extension_for(content_type: &str) -> Option<&'static str> {
    match content_type {
        "video/mp4" => Some("mp4"),
        "video/webm" => Some("webm"),
        _ => None,
    }
}

fn host_of(state: &AppState) -> &str {
    &state.config().server.host
}

async fn resolve_local_actor(state: &AppState) -> Result<ActorRow, Response> {
    let host = &state.config().server.host;
    let user = &state.config().server.user;
    let row = repo::actor::get_by_username_host(state.pool(), user, host)
        .await
        .map_err(|err| {
            error!(?err, "media upload: local actor lookup failed");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        })?;
    match row {
        Some(a) if a.is_local => Ok(a),
        _ => Err(error_with_body(
            StatusCode::SERVICE_UNAVAILABLE,
            "local actor not initialized; run `sakurasato init`",
        )),
    }
}

fn map_proxy_error(err: &MediaProxyError) -> Response {
    match err {
        MediaProxyError::Timeout(_) => {
            error_with_body(StatusCode::GATEWAY_TIMEOUT, &err.to_string())
        }
        MediaProxyError::Transport(_) | MediaProxyError::MissingContentType => {
            warn!(?err, "media-proxy transport failure");
            error_with_body(StatusCode::BAD_GATEWAY, &err.to_string())
        }
        MediaProxyError::TooLarge => {
            error_with_body(StatusCode::PAYLOAD_TOO_LARGE, &err.to_string())
        }
        MediaProxyError::Upstream {
            status,
            reason,
            message,
        } => {
            let mapped = match status.as_u16() {
                400 | 403 => StatusCode::BAD_REQUEST,
                413 => StatusCode::PAYLOAD_TOO_LARGE,
                415 => StatusCode::UNSUPPORTED_MEDIA_TYPE,
                504 => StatusCode::GATEWAY_TIMEOUT,
                _ => StatusCode::BAD_GATEWAY,
            };
            error_with_body(mapped, &format!("{reason}: {message}"))
        }
    }
}

fn bad_request(reason: &'static str) -> Response {
    error_with_body(StatusCode::BAD_REQUEST, reason)
}

fn error_with_body(status: StatusCode, reason: &str) -> Response {
    (status, Json(json!({ "error": reason }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variant_mapping_covers_all_kinds() {
        assert_eq!(variant_for("avatar"), Some("avatar"));
        assert_eq!(variant_for("header"), Some("header"));
        assert_eq!(variant_for("attachment"), Some("preview"));
        assert_eq!(variant_for("preview"), None);
        assert_eq!(variant_for(""), None);
        assert_eq!(variant_for("AVATAR"), None);
    }

    #[test]
    fn build_media_url_works() {
        assert_eq!(
            build_media_url("example.test", "abc.webp"),
            "https://example.test/media/abc.webp"
        );
    }
}
