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
//! - `POST /api/drive/files/update { fileId, comment? }` ── `comment` (= alt text)
//!   を更新。`name`/`isSensitive`/`folderId` は受理するが `media` 行に対応列が
//!   無いため永続化しない (sensitive は AP 伝搬込みの別 issue)。`write:drive`。
//! - `POST /api/drive/files/delete { fileId }` ── **未添付** file を削除。添付済み
//!   は 400 (note のスナップショット参照を壊さない)。`write:drive`、204 返却。
//! - `POST /api/drive` ── `DriveUsage` `{ capacity, usage }` (bytes)。Aria のドライブ
//!   タブヘッダ用。`capacity = 0` (= 容量無制限、meta の `driveCapacityMb: 0` と
//!   整合)、`usage` は自分の media の合計サイズ。`read:drive`。
//! - `POST /api/drive/folders` ── フォルダ一覧。Sakurasato は Misskey の "フォルダ"
//!   概念を持たない (= 添付は note に紐付くだけ) ので **常に空配列**。`read:drive`。
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
use serde::{Deserialize, Serialize};

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

/// `POST /api/drive/files/update` ── `DriveFile` のメタデータ更新。Sakurasato の
/// `media` 行は可変属性として `alt_text` しか持たないので、永続化するのは
/// `comment` (= AP alt text) のみ。`name` / `isSensitive` / `folderId` は受理して
/// 200 を返すが書き込まない (モデルに対応列が無い ── sensitive は AP 伝搬込みで
/// 別 issue)。`write:drive` scope、所有者ガード。
///
/// `comment` の意味は Misskey 仕様に倣う ── **不在なら据え置き**、`null` か空文字
/// なら **クリア (NULL)**、文字列なら **設定**。present / absent を区別するため body
/// は [`serde_json::Value`] で受ける (`Option<String>` だと両者を見分けられない)。
pub async fn update(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<serde_json::Value>>,
) -> Response {
    let body = body.map_or(serde_json::Value::Null, |j| j.0);
    let token = body.get("i").and_then(serde_json::Value::as_str);
    if auth::require_scope(&state, &headers, token, SCOPE_WRITE_DRIVE)
        .await
        .is_none()
    {
        return auth::unauthorized("invalid or revoked token");
    }
    let Some(owner) = local_actor_id(&state).await else {
        return internal_error("local actor not initialized");
    };
    // fileId 不在 / 非数値はどちらも「そんな file は無い」に倒す。
    let Some(file_id) = body
        .get("fileId")
        .and_then(serde_json::Value::as_str)
        .and_then(|s| s.parse::<i64>().ok())
    else {
        return error_resp(StatusCode::NOT_FOUND, "NO_SUCH_FILE", "no such file");
    };

    // comment: None = 据え置き / Some(None) = クリア / Some(Some) = 設定。
    let comment_change: Option<Option<String>> = match body.get("comment") {
        None => None,
        Some(serde_json::Value::Null) => Some(None),
        Some(serde_json::Value::String(s)) if s.is_empty() => Some(None),
        Some(serde_json::Value::String(s)) => {
            if s.chars().count() > COMMENT_MAX {
                return bad_request("comment exceeds the 1500-character limit");
            }
            Some(Some(s.clone()))
        }
        Some(_) => return bad_request("comment must be a string"),
    };

    let updated = match comment_change {
        Some(new_alt) => {
            match repo::media::set_alt_text_for_owner(
                state.pool(),
                file_id,
                owner,
                new_alt.as_deref(),
            )
            .await
            {
                Ok(Some(row)) => row,
                Ok(None) => {
                    return error_resp(StatusCode::NOT_FOUND, "NO_SUCH_FILE", "no such file");
                }
                Err(err) => {
                    tracing::error!(?err, file_id, "drive/files/update failed");
                    return internal_error("drive file update failed");
                }
            }
        }
        // 永続化する変更なし ── 所有者ガードのうえ現在の file をそのまま返す。
        None => match repo::media::get_by_id_for_owner(state.pool(), file_id, owner).await {
            Ok(Some(row)) => row,
            Ok(None) => return error_resp(StatusCode::NOT_FOUND, "NO_SUCH_FILE", "no such file"),
            Err(err) => {
                tracing::error!(?err, file_id, "drive/files/update lookup failed");
                return internal_error("drive file lookup failed");
            }
        },
    };

    let host = &state.config().server.host;
    Json(media_row_to_miss_file(&updated, host)).into_response()
}

#[derive(Debug, Deserialize, Default)]
pub struct DeleteBody {
    #[serde(default)]
    pub i: Option<String>,
    #[serde(rename = "fileId", default)]
    pub file_id: Option<String>,
}

/// `POST /api/drive/files/delete { fileId }` ── 自分の **未添付** file を削除する。
/// `write:drive` scope、所有者ガード。添付済み (= どこかの note が参照) は
/// `400` `FILE_ATTACHED` で拒否する ── `note.attachments` JSONB スナップショットが
/// `storage_key` を握っているので、消すと既存 note の画像参照が壊れるため。添付を
/// 消したいときは note ごと削除する。成功は **204 No Content** (Misskey 仕様)。
pub async fn delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<DeleteBody>>,
) -> Response {
    let body = body.map(|j| j.0).unwrap_or_default();
    if auth::require_scope(&state, &headers, body.i.as_deref(), SCOPE_WRITE_DRIVE)
        .await
        .is_none()
    {
        return auth::unauthorized("invalid or revoked token");
    }
    let Some(owner) = local_actor_id(&state).await else {
        return internal_error("local actor not initialized");
    };
    let Some(file_id) = body.file_id.as_deref().and_then(|s| s.parse::<i64>().ok()) else {
        return error_resp(StatusCode::NOT_FOUND, "NO_SUCH_FILE", "no such file");
    };

    // storage_key 取得 + 添付チェックのため先に引く (所有者ガード込み)。
    let row = match repo::media::get_by_id_for_owner(state.pool(), file_id, owner).await {
        Ok(Some(r)) => r,
        Ok(None) => return error_resp(StatusCode::NOT_FOUND, "NO_SUCH_FILE", "no such file"),
        Err(err) => {
            tracing::error!(?err, file_id, "drive/files/delete lookup failed");
            return internal_error("drive file lookup failed");
        }
    };
    if row.note_id.is_some() {
        return error_resp(
            StatusCode::BAD_REQUEST,
            "FILE_ATTACHED",
            "file is attached to a note; delete the note instead",
        );
    }

    // DB 行を先に消す (= 権威)。R2 削除が失敗して行だけ残り参照先が消える事故より、
    // R2 に orphan object が残る方がマシ (= 後で GC 可能)。
    match repo::media::delete_unattached_for_owner(state.pool(), file_id, owner).await {
        Ok(true) => {}
        // get と delete の間に attach された等のレア競合。改めて添付扱いで弾く。
        Ok(false) => {
            return error_resp(
                StatusCode::BAD_REQUEST,
                "FILE_ATTACHED",
                "file is attached to a note; delete the note instead",
            );
        }
        Err(err) => {
            tracing::error!(?err, file_id, "drive/files/delete failed");
            return internal_error("drive file delete failed");
        }
    }

    // R2 オブジェクトを best-effort 削除。storage_key は UNIQUE なので他行と共有
    // せず、この削除で別 file が壊れることはない。失敗しても row は既に消えている
    // ので 204 を返す (orphan object のみ残る)。
    let bucket = state.config().storage.bucket.clone();
    if let Err(err) = state
        .s3_client()
        .delete_object()
        .bucket(&bucket)
        .key(&row.storage_key)
        .send()
        .await
    {
        tracing::warn!(
            ?err,
            storage_key = %row.storage_key,
            "drive/files/delete: object store delete failed (row already removed)"
        );
    }

    StatusCode::NO_CONTENT.into_response()
}

/// `i` (token) だけを取る body。`drive` (usage) / `drive/folders` が共有する。
#[derive(Debug, Deserialize, Default)]
pub struct TokenOnlyBody {
    #[serde(default)]
    pub i: Option<String>,
}

/// Misskey `DriveUsage` ── `{ capacity, usage }` (bytes)。
#[derive(Debug, Serialize)]
struct DriveUsage {
    capacity: i64,
    usage: i64,
}

/// `POST /api/drive` ── ドライブ使用量 `{ capacity, usage }` (bytes)。Aria の
/// ドライブタブヘッダの使用量バー用。`capacity = 0` は meta の `driveCapacityMb: 0`
/// と揃えた「容量無制限」(= R2 backed のお一人様で per-user quota を課さない)。
/// `usage` は自分の media の `byte_size` 合計。`read:drive` scope。
pub async fn usage(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<TokenOnlyBody>>,
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
    match repo::media::total_byte_size_for_owner(state.pool(), owner).await {
        // capacity 0 = 無制限 (meta の driveCapacityMb: 0 と整合)。
        Ok(usage) => Json(DriveUsage { capacity: 0, usage }).into_response(),
        Err(err) => {
            tracing::error!(?err, "drive usage aggregate failed");
            internal_error("drive usage failed")
        }
    }
}

/// `POST /api/drive/folders` ── フォルダ一覧。Sakurasato は Misskey の "フォルダ"
/// 概念を持たない (= 添付は note に紐付くだけ) ので **常に空配列** を返す。Aria の
/// ドライブタブはこれで「フォルダ無し」を表示して開ける。`folderId` / `limit` 等の
/// param は受理しても無視 (空配列なので意味を持たない)。`read:drive` scope。
pub async fn folders(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<TokenOnlyBody>>,
) -> Response {
    let body = body.map(|j| j.0).unwrap_or_default();
    if auth::require_scope(&state, &headers, body.i.as_deref(), SCOPE_READ_DRIVE)
        .await
        .is_none()
    {
        return auth::unauthorized("invalid or revoked token");
    }
    Json(Vec::<serde_json::Value>::new()).into_response()
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
