//! `MiAuth` `drive/files/*` ── Misskey 互換のドライブ (= 添付ファイル) API。
//!
//! Aria / Milktea 等は投稿に画像を添付する際、まず `POST /api/drive/files/create`
//! でファイルをアップロードして `DriveFile` を得て、その `id` を
//! `notes/create` の `fileIds` に渡す。本 module はその `MiAuth` 面の配線で、
//! 実体のアップロード処理 (media-proxy サニタイズ → R2 PUT → `media` 行 INSERT)
//! は TUI 用 local API と共通の [`crate::local_api::media::upload_media_core`] を
//! 呼ぶ ── face が違うだけでパイプラインは 1 本。
//!
//! ## エンドポイント
//!
//! - `POST /api/drive/files/create` (multipart) ── `file` をアップロードし
//!   `DriveFile` (`MissFile`) を返す。`write:drive` scope。
//! - `POST /api/drive/files` ── 自分の drive ファイル一覧。`read:drive`。
//! - `POST /api/drive/files/show { fileId }` ── 単一 `DriveFile`。`read:drive`。
//!
//! ## clean-room
//!
//! wire は <https://api-doc.misskey.io/> 記載の `DriveFile` / `drive/files/create`
//! を一次資料とし、Misskey 本体 (AGPL) の handler は参照しない
//! ([[agpl-discipline-miauth]])。`DriveFile` schema は [`crate::miauth::conv::MissFile`]
//! (misskey-dart 準拠) を再利用する。

use axum::Json;
use axum::extract::{Multipart, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use sakurasato_core::repo;
use serde::Deserialize;

use crate::local_api;
use crate::miauth::auth;
use crate::miauth::conv::{MissFile, media_row_to_miss_file};
use crate::miauth::error::{bad_request, error_resp, internal_error};
use crate::state::AppState;

/// `drive/files/create` 等の書き込み scope (Misskey 仕様)。
const SCOPE_WRITE_DRIVE: &str = "write:drive";
/// `drive/files` / `drive/files/show` の読み取り scope。
const SCOPE_READ_DRIVE: &str = "read:drive";

const LIST_LIMIT_DEFAULT: i64 = 10;
const LIST_LIMIT_MAX: i64 = 100;
/// `comment` (= AP alt text) の最大文字数。`local_api` 側 upload と揃える。
const COMMENT_MAX: usize = 1500;

/// `POST /api/drive/files/create` ── multipart `file` をアップロードし `DriveFile`
/// を返す。Aria はこの `id` を `notes/create` の `fileIds` に使う。
pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Response {
    // multipart を走査して i (token) / file (本体) / comment (alt) を集める。
    // name / isSensitive / folderId 等は受けても drive 行には保持しないので
    // drain する (Sakurasato の media 行は sensitive を note 側で持つ)。
    let mut token: Option<String> = None;
    let mut file: Option<Bytes> = None;
    let mut comment: Option<String> = None;
    loop {
        match multipart.next_field().await {
            Ok(Some(field)) => {
                let name = field.name().map(str::to_string);
                match name.as_deref() {
                    Some("i") => token = field.text().await.ok(),
                    Some("file") => file = field.bytes().await.ok(),
                    Some("comment") => {
                        comment = field.text().await.ok().filter(|s| !s.is_empty());
                    }
                    _ => {
                        // 未使用 field も読み切らないと次に進めない。
                        let _ = field.bytes().await;
                    }
                }
            }
            Ok(None) => break,
            Err(err) => {
                tracing::warn!(?err, "drive/files/create: multipart read failed");
                return bad_request("malformed multipart/form-data body");
            }
        }
    }

    // token は form field `i` か Authorization ヘッダのどちらでも可。
    if auth::require_scope(&state, &headers, token.as_deref(), SCOPE_WRITE_DRIVE)
        .await
        .is_none()
    {
        return auth::unauthorized("invalid or revoked token");
    }

    let Some(bytes) = file else {
        return bad_request("file is required");
    };
    if let Some(c) = comment.as_deref()
        && c.chars().count() > COMMENT_MAX
    {
        return bad_request("comment exceeds the 1500-character limit");
    }

    // 既存パイプラインを共有 (kind = attachment)。エラー時の Response は
    // local_api 形 (status は意味のあるもの) をそのまま返す ── 成功パスの
    // DriveFile は完全に Misskey 形。
    match local_api::media::upload_media_core(&state, "attachment", comment.as_deref(), bytes).await
    {
        Ok((row, _created)) => {
            let host = &state.config().server.host;
            Json(media_row_to_miss_file(&row, host)).into_response()
        }
        Err(resp) => resp,
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct ListBody {
    #[serde(default)]
    pub i: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(rename = "sinceId", default)]
    pub since_id: Option<String>,
    #[serde(rename = "untilId", default)]
    pub until_id: Option<String>,
}

/// `POST /api/drive/files` ── 自分の drive ファイル一覧 (id 降順、排他カーソル)。
pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<ListBody>>,
) -> Response {
    let body = body.map(|j| j.0).unwrap_or_default();
    if auth::require_scope(&state, &headers, body.i.as_deref(), SCOPE_READ_DRIVE)
        .await
        .is_none()
    {
        return auth::unauthorized("invalid or revoked token");
    }
    let Some(owner) = local_actor_id(&state).await else {
        return internal_error("local actor not initialized");
    };
    let limit = body
        .limit
        .unwrap_or(LIST_LIMIT_DEFAULT)
        .clamp(1, LIST_LIMIT_MAX);
    let since = body.since_id.as_deref().and_then(|s| s.parse::<i64>().ok());
    let until = body.until_id.as_deref().and_then(|s| s.parse::<i64>().ok());
    match repo::media::list_by_owner_window(state.pool(), owner, since, until, limit).await {
        Ok(rows) => {
            let host = &state.config().server.host;
            let files: Vec<MissFile> = rows
                .iter()
                .map(|r| media_row_to_miss_file(r, host))
                .collect();
            Json(files).into_response()
        }
        Err(err) => {
            tracing::error!(?err, "drive/files: list failed");
            internal_error("drive list failed")
        }
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct ShowBody {
    #[serde(default)]
    pub i: Option<String>,
    #[serde(rename = "fileId", default)]
    pub file_id: Option<String>,
}

/// `POST /api/drive/files/show { fileId }` ── 単一 `DriveFile` (所有者ガード込み)。
pub async fn show(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<ShowBody>>,
) -> Response {
    let body = body.map(|j| j.0).unwrap_or_default();
    if auth::require_scope(&state, &headers, body.i.as_deref(), SCOPE_READ_DRIVE)
        .await
        .is_none()
    {
        return auth::unauthorized("invalid or revoked token");
    }
    let Some(owner) = local_actor_id(&state).await else {
        return internal_error("local actor not initialized");
    };
    let Some(file_id_str) = body.file_id else {
        return bad_request("fileId is required");
    };
    let Ok(file_id) = file_id_str.parse::<i64>() else {
        return error_resp(StatusCode::NOT_FOUND, "NO_SUCH_FILE", "no such file");
    };
    match repo::media::get_by_id_for_owner(state.pool(), file_id, owner).await {
        Ok(Some(row)) => {
            let host = &state.config().server.host;
            Json(media_row_to_miss_file(&row, host)).into_response()
        }
        Ok(None) => error_resp(StatusCode::NOT_FOUND, "NO_SUCH_FILE", "no such file"),
        Err(err) => {
            tracing::error!(?err, file_id, "drive/files/show: lookup failed");
            internal_error("drive file lookup failed")
        }
    }
}

/// 設定の `[server].user@host` から local actor の id を引く (drive の所有者)。
async fn local_actor_id(state: &AppState) -> Option<i64> {
    let host = &state.config().server.host;
    let user = &state.config().server.user;
    match repo::actor::get_by_username_host(state.pool(), user, host).await {
        Ok(Some(a)) if a.is_local => Some(a.id),
        _ => None,
    }
}
