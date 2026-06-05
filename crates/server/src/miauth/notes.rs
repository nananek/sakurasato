//! `POST /api/notes/show` / `POST /api/notes/timeline` (= M14 #159, 親 issue #150)。
//!
//! Misskey 互換クライアント (Milktea / `MissRirica` 等) が Note を取得する
//! 主要経路。本 module は **読み取り専用** で、scope は `read:account` を要求する。
//!
//! ## wire 仕様の出典 (clean-room)
//!
//! - <https://api-doc.misskey.io/> (= `/api-doc` の `OpenAPI`)
//! - <https://misskey-hub.net/>
//!
//! Misskey の TypeScript handler は読まずに、observed wire shape (=
//! `misskey-py` で本物に問い合わせて確認) を pytest parity test
//! (`tests/federation/test_miauth_read_parity.py`) で常時 assert する。
//! AGPL discipline は [[agpl-discipline-miauth]] / [`crate::miauth`] module doc
//! 参照。
//!
//! ## エンドポイント
//!
//! - `POST /api/notes/show { i, noteId }` ── 単一 Note を `MissNote` で返す
//! - `POST /api/notes/timeline { i, limit?, sinceId?, untilId?, sinceDate?, untilDate? }`
//!   ── home timeline (= 既存 `repo::note::list_home_timeline` の Misskey 互換窓)
//!
//! ## N+1 回避
//!
//! `notes/timeline` は per-note の reaction / announce / emoji を **bulk loader**
//! ([`crate::miauth::conv::bulk_load_note_summaries`]) で 2 query にまとめる。
//! 10 件 timeline でも DB round-trip は note SELECT + reactions + announces の
//! 計 3 本に収まる。

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, TimeZone, Utc};
use http_body_util::BodyExt;
use sakurasato_core::repo;
use serde::Deserialize;
use serde_json::{Value as JsonValue, json};

use crate::local_api;
use crate::miauth::auth;
use crate::miauth::conv::{
    MissNote, NoteSummary, build_renote_miss_note, bulk_load_note_summaries, from_actor_and_counts,
    timeline_entry_to_miss_note,
};
use crate::state::AppState;

/// `read:account` scope (= Misskey 仕様で `notes/timeline` / `notes/show` /
/// `users/show` 共通の最小権限)。
const SCOPE_READ_ACCOUNT: &str = "read:account";

/// `write:notes` scope (= notes/create, notes/delete, notes/renote 共通)。
const SCOPE_WRITE_NOTES: &str = "write:notes";

/// `limit` の既定値。Misskey 公式 default = 10。
const TIMELINE_LIMIT_DEFAULT: i64 = 10;
/// `limit` の上限。Misskey 公式 max = 100。
const TIMELINE_LIMIT_MAX: i64 = 100;

#[derive(Debug, Deserialize, Default)]
pub struct ShowBody {
    #[serde(default)]
    pub i: Option<String>,
    #[serde(rename = "noteId", default)]
    pub note_id: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct TimelineBody {
    #[serde(default)]
    pub i: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    /// **排他下限** ── `WHERE id > sinceId`。Misskey 公式準拠 (Sakurasato 内
    /// `note.id` は `BIGSERIAL` ─ Misskey の `aidx` とは違うが string で受ける)。
    #[serde(rename = "sinceId", default)]
    pub since_id: Option<String>,
    /// **排他上限** ── `WHERE id < untilId`。
    #[serde(rename = "untilId", default)]
    pub until_id: Option<String>,
    /// **排他下限 (時刻)** ── 単位は **ms epoch**。
    #[serde(rename = "sinceDate", default)]
    pub since_date: Option<i64>,
    /// **排他上限 (時刻)** ── 単位は **ms epoch**。
    #[serde(rename = "untilDate", default)]
    pub until_date: Option<i64>,
}

/// `POST /api/notes/show` handler。
pub async fn show(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<ShowBody>>,
) -> Response {
    let body = body.map(|j| j.0).unwrap_or_default();
    let Some(_token_row) =
        auth::require_scope(&state, &headers, body.i.as_deref(), SCOPE_READ_ACCOUNT).await
    else {
        return auth::unauthorized("invalid or revoked token");
    };
    let Some(note_id_str) = body.note_id else {
        return bad_request("noteId is required");
    };
    let Ok(note_id) = note_id_str.parse::<i64>() else {
        return error_with_status(StatusCode::NOT_FOUND, "NO_SUCH_NOTE", "no such note");
    };

    let entry = match repo::note::get_timeline_entry_by_id(state.pool(), note_id).await {
        Ok(Some(e)) => e,
        Ok(None) => {
            return error_with_status(StatusCode::NOT_FOUND, "NO_SUCH_NOTE", "no such note");
        }
        Err(err) => {
            tracing::error!(
                ?err,
                note_id,
                "miauth notes/show: get_timeline_entry_by_id failed"
            );
            return error_with_status(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "note lookup failed",
            );
        }
    };

    let Some(viewer) = resolve_self_actor_id(&state).await else {
        return error_with_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "local actor initialization failed",
        );
    };

    // direct visibility は本人 or audience 含まれている時のみ見える。
    // ここでは「お一人様 server で `direct` 受信は自分宛のみ」という現状を
    // 利用し、`actor_id == viewer` または `viewer` の ap_id が to/cc に含まれる
    // ときに通す。
    if entry.visibility == "direct" {
        let viewer_uri = viewer_ap_id(&state).await;
        let allowed = entry.actor_id == viewer
            || viewer_uri.as_ref().is_some_and(|uri| {
                entry
                    .to_recipients
                    .0
                    .iter()
                    .chain(entry.cc_recipients.0.iter())
                    .any(|r| r == uri)
            });
        if !allowed {
            return error_with_status(StatusCode::NOT_FOUND, "NO_SUCH_NOTE", "no such note");
        }
    }
    if entry.visibility == "followers" && entry.actor_id != viewer {
        // viewer が author を accepted で follow しているか確認。
        let follows = match repo::follow::get_by_pair(state.pool(), viewer, entry.actor_id).await {
            Ok(Some(f)) => f.state == "accepted",
            _ => false,
        };
        if !follows {
            return error_with_status(StatusCode::NOT_FOUND, "NO_SUCH_NOTE", "no such note");
        }
    }

    let summaries = bulk_load_note_summaries(state.pool(), &[entry.id], viewer).await;
    let summary = summaries.remove_summary(entry.id);
    let host = &state.config().server.host;
    let note = timeline_entry_to_miss_note(&entry, &summary, host, viewer);
    Json(note).into_response()
}

/// `POST /api/notes/timeline` handler。
pub async fn timeline(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<TimelineBody>>,
) -> Response {
    let body = body.map(|j| j.0).unwrap_or_default();
    let Some(_token_row) =
        auth::require_scope(&state, &headers, body.i.as_deref(), SCOPE_READ_ACCOUNT).await
    else {
        return auth::unauthorized("invalid or revoked token");
    };

    let Some(viewer) = resolve_self_actor_id(&state).await else {
        return error_with_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "local actor initialization failed",
        );
    };

    let limit = body
        .limit
        .unwrap_or(TIMELINE_LIMIT_DEFAULT)
        .clamp(1, TIMELINE_LIMIT_MAX);
    let since_id = parse_id_opt(body.since_id.as_deref());
    let until_id = parse_id_opt(body.until_id.as_deref());
    let since_date = body.since_date.and_then(ms_epoch_to_datetime);
    let until_date = body.until_date.and_then(ms_epoch_to_datetime);

    let entries = match repo::note::list_home_timeline_window(
        state.pool(),
        viewer,
        since_id,
        until_id,
        since_date,
        until_date,
        limit,
    )
    .await
    {
        Ok(v) => v,
        Err(err) => {
            tracing::error!(
                ?err,
                "miauth notes/timeline: list_home_timeline_window failed"
            );
            return error_with_status(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "timeline query failed",
            );
        }
    };

    let note_ids: Vec<i64> = entries.iter().map(|e| e.id).collect();
    let mut summaries = bulk_load_note_summaries(state.pool(), &note_ids, viewer).await;

    let host = &state.config().server.host;
    let notes: Vec<MissNote> = entries
        .iter()
        .map(|e| {
            let summary = summaries.remove(&e.id).unwrap_or(NoteSummary {
                reactions: Vec::new(),
                announce: None,
            });
            timeline_entry_to_miss_note(e, &summary, host, viewer)
        })
        .collect();
    Json(notes).into_response()
}

/// local actor の id を引く。お一人様サーバ前提で 1 件しかない。
async fn resolve_self_actor_id(state: &AppState) -> Option<i64> {
    let host = &state.config().server.host;
    let user = &state.config().server.user;
    match repo::actor::get_by_username_host(state.pool(), user, host).await {
        Ok(Some(row)) if row.is_local => Some(row.id),
        _ => None,
    }
}

/// local actor の `ap_id` を引く (= direct visibility 判定で audience に含まれる
/// か確認するため)。
async fn viewer_ap_id(state: &AppState) -> Option<String> {
    let host = &state.config().server.host;
    let user = &state.config().server.user;
    repo::actor::get_by_username_host(state.pool(), user, host)
        .await
        .ok()
        .flatten()
        .map(|a| a.ap_id)
}

/// Misskey は noteId / userId を **string** で渡すが、Sakurasato は内部で
/// `i64` なので parse 必須。失敗時は `None` を返し、handler で
/// `404 NO_SUCH_NOTE` 等に倒す ── Misskey 慣行で「無効な ID は 404」相当。
fn parse_id_opt(s: Option<&str>) -> Option<i64> {
    s.and_then(|t| t.parse::<i64>().ok())
}

/// `sinceDate`/`untilDate` は **ms epoch** で渡される (Misskey 仕様)。
/// 範囲外 (= `i64::MAX` を超える / sec 換算で範囲外) は `None` に倒す。
fn ms_epoch_to_datetime(ms: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_millis_opt(ms).single()
}

/// `error.code` / `error.message` 形式の Misskey 互換エラーレスポンス。
fn error_with_status(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({
            "error": {
                "code": code,
                "message": message,
            },
        })),
    )
        .into_response()
}

fn bad_request(message: &str) -> Response {
    error_with_status(StatusCode::BAD_REQUEST, "INVALID_PARAM", message)
}

// ─── #160: write endpoints (notes/create, notes/delete, notes/renote) ───────

/// Misskey `notes/create` body。
///
/// 一部フィールドは未対応 ── `poll` (= 投票) / `channelId` (= Misskey の channel)
/// / `localOnly` (= LTL 限定) / `noExtractMentions` (= mention 抽出 OFF) は wire
/// shape として受理するが、本 PR では無視する。実 Misskey クライアントの利用
/// 頻度が低い + Sakurasato コアモデルに該当概念が無いため。
#[derive(Debug, Deserialize, Default)]
pub struct CreateNoteBody {
    #[serde(default)]
    pub i: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub cw: Option<String>,
    /// Misskey: `public` / `home` (= unlisted) / `followers` / `specified` (= direct)。
    /// 未指定なら `public`。
    #[serde(default)]
    pub visibility: Option<String>,
    #[serde(rename = "replyId", default)]
    pub reply_id: Option<String>,
    /// renoteId が指定されると Announce 経路に倒れる (本 PR では未対応 ──
    /// 既存 `/api/v1/notes/{id}/renote` 経路と統合する task は別 PR)。
    /// 互換のため body 受理はするが実装は 501 を返す。
    #[serde(rename = "renoteId", default)]
    pub renote_id: Option<String>,
    /// 添付メディア id 配列。Misskey は string 配列を渡すので i64 に parse する。
    #[serde(rename = "fileIds", default)]
    pub file_ids: Vec<String>,
}

/// `POST /api/notes/create` handler。
pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<CreateNoteBody>>,
) -> Response {
    let body = body.map(|j| j.0).unwrap_or_default();
    let Some(_token) =
        auth::require_scope(&state, &headers, body.i.as_deref(), SCOPE_WRITE_NOTES).await
    else {
        return auth::unauthorized("invalid or revoked token");
    };

    // **pure renote** (= 本文なし + renoteId あり) → Announce (boost)。既存
    // `local_api::renotes::create` (= announce 行 + 連合 Announce 配送) に通し、
    // Misskey の renote MissNote 形にレスポンスを合成する。
    if body.text.as_deref().is_none_or(str::is_empty) && body.renote_id.is_some() {
        return handle_renote(&state, body.renote_id.as_deref()).await;
    }
    // **quote renote** (= renoteId + 本文) は未対応 (= 引用は note に別 note を
    // 参照させる別概念で core 未実装)。silent に renote 関係を落として単独 note
    // 化しないよう、明示的に 501 を返す。
    if body.renote_id.is_some() {
        return error_with_status(
            StatusCode::NOT_IMPLEMENTED,
            "QUOTE_NOT_IMPLEMENTED",
            "quote renote (renoteId + text) is not implemented in this version",
        );
    }

    // **replyId** (= Aria 等が返信投稿で送ってくる MissNote.id = Sakurasato note の
    // i64 id stringify) を親 note の `ap_id` に解決し、`local_api::notes::create` の
    // `in_reply_to_ap_id` 経路に接続する。親作者の mention / 配送先解決は
    // local_api 側 [`crate::local_api::notes`] の `resolve_reply_parent` が担当 (#64)。
    let in_reply_to_ap_id = match body.reply_id.as_deref() {
        None => None,
        Some(rid) => {
            let Ok(reply_note_id) = rid.parse::<i64>() else {
                return error_with_status(
                    StatusCode::BAD_REQUEST,
                    "INVALID_PARAM",
                    "replyId is not a valid note id",
                );
            };
            match repo::note::get_by_id(state.pool(), reply_note_id).await {
                Ok(Some(parent)) => Some(parent.ap_id),
                Ok(None) => {
                    return error_with_status(
                        StatusCode::BAD_REQUEST,
                        "NO_SUCH_REPLY_TARGET",
                        "no such reply target",
                    );
                }
                Err(err) => {
                    tracing::error!(
                        ?err,
                        reply_note_id,
                        "miauth notes/create: reply target lookup failed"
                    );
                    return error_with_status(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "INTERNAL_ERROR",
                        "reply target lookup failed",
                    );
                }
            }
        }
    };

    let internal_req = match translate_create_body(&body, in_reply_to_ap_id) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    // 既存 [`local_api::notes::create`] を直接呼ぶ ── 中身は重い実装で複製を避ける。
    // Response 経由でやり取りするので body を読み戻して MissNote に詰め替える。
    let resp = local_api::notes::create(State(state.clone()), Json(internal_req)).await;
    let status = resp.status();
    let body_json = match collect_json(resp).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    if !status.is_success() {
        return translate_local_error_to_misskey(status, &body_json);
    }
    let Some(note_id) = body_json.get("id").and_then(JsonValue::as_i64) else {
        tracing::error!(
            ?body_json,
            "miauth notes/create: missing id in inner response"
        );
        return error_with_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "note creation failed; missing id",
        );
    };

    let Ok(Some(entry)) = repo::note::get_timeline_entry_by_id(state.pool(), note_id).await else {
        tracing::error!(
            note_id,
            "miauth notes/create: get_timeline_entry_by_id failed after create"
        );
        return error_with_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "note lookup failed after creation",
        );
    };
    let Some(viewer) = resolve_self_actor_id(&state).await else {
        return error_with_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "local actor initialization failed",
        );
    };
    let summaries = bulk_load_note_summaries(state.pool(), &[entry.id], viewer).await;
    let summary = summaries.remove_summary(entry.id);
    let host = &state.config().server.host;
    let note = timeline_entry_to_miss_note(&entry, &summary, host, viewer);

    // Misskey wire は `{ createdNote: MissNote }` を返す。
    (StatusCode::OK, Json(json!({ "createdNote": note }))).into_response()
}

#[derive(Debug, Deserialize, Default)]
pub struct DeleteNoteBody {
    #[serde(default)]
    pub i: Option<String>,
    #[serde(rename = "noteId", default)]
    pub note_id: Option<String>,
}

/// `POST /api/notes/delete` handler。
///
/// 自分が author の note を削除し、Delete activity をフォロワー全員に配送する。
pub async fn delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<DeleteNoteBody>>,
) -> Response {
    let body = body.map(|j| j.0).unwrap_or_default();
    let Some(_token) =
        auth::require_scope(&state, &headers, body.i.as_deref(), SCOPE_WRITE_NOTES).await
    else {
        return auth::unauthorized("invalid or revoked token");
    };
    let Some(note_id) = body.note_id.as_deref().and_then(|s| s.parse::<i64>().ok()) else {
        return error_with_status(StatusCode::NOT_FOUND, "NO_SUCH_NOTE", "no such note");
    };

    let Some(viewer) = resolve_self_actor_id(&state).await else {
        return error_with_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "local actor initialization failed",
        );
    };
    let note = match repo::note::get_by_id(state.pool(), note_id).await {
        Ok(Some(n)) => n,
        Ok(None) => {
            return error_with_status(StatusCode::NOT_FOUND, "NO_SUCH_NOTE", "no such note");
        }
        Err(err) => {
            tracing::error!(?err, note_id, "miauth notes/delete: lookup failed");
            return error_with_status(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "note lookup failed",
            );
        }
    };
    if note.actor_id != viewer {
        return error_with_status(
            StatusCode::FORBIDDEN,
            "PERMISSION_DENIED",
            "note not owned by you",
        );
    }
    if !note.is_local {
        // remote note を local が削除しようとしても AP 上は無意味 (= 相手側の note)。
        return error_with_status(
            StatusCode::FORBIDDEN,
            "PERMISSION_DENIED",
            "cannot delete a remote note",
        );
    }

    // 配送先 = followers 全員 (= 投稿時の to/cc inbox 解決と同じ流儀)。
    let inboxes = match repo::follow::list_accepted_inboxes(state.pool(), viewer).await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!(?err, "miauth notes/delete: list_accepted_inboxes failed");
            Vec::new()
        }
    };
    let Ok(Some(local_actor)) = sakurasato_core::repo::actor::get_by_id(state.pool(), viewer).await
    else {
        return error_with_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "local actor fetch failed",
        );
    };
    let delete_activity = build_delete_note_activity(&local_actor, &note);

    let mut queued = 0_usize;
    for inbox in &inboxes {
        match crate::delivery::enqueue_activity(state.pool(), viewer, inbox, &delete_activity).await
        {
            Ok(_) => queued += 1,
            Err(err) => tracing::warn!(?err, %inbox, "miauth notes/delete: enqueue failed"),
        }
    }
    if queued > 0 {
        state.wake_delivery();
    }
    let _ = queued; // queued は wire には載せない (Misskey wire は 204)。

    // DB から削除 (= note 本体)。reaction / announce は FK CASCADE で外れる前提。
    if let Err(err) = repo::note::delete_by_ap_id(state.pool(), &note.ap_id).await {
        tracing::warn!(
            ?err,
            note_id,
            "miauth notes/delete: row delete failed (Delete already queued)"
        );
    }

    (StatusCode::NO_CONTENT, ()).into_response()
}

/// `POST /api/notes/renote` (Iceshrimp 互換 alias) handler。
///
/// Misskey 本体は `notes/renote` を持たず、`notes/create with renoteId` で代用する。
/// Iceshrimp / Sharkey 系は alias として提供しているので、Sakurasato も同形で
/// 受ける ── 内部実装は `notes/create` に forward し、`create()` 側の pure renote
/// (= Announce) 経路を流用する。
pub async fn renote(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<CreateNoteBody>>,
) -> Response {
    create(State(state), headers, body).await
}

/// **pure renote** (= Announce / boost) を処理する。`renoteId` の note を既存
/// [`crate::local_api::notes`] 隣の `local_api::renotes::create` (= announce 行 +
/// 連合 `Announce` 配送) に通し、Misskey の renote `MissNote` 形にレスポンスを
/// 合成して返す。
///
/// Sakurasato は renote を `announce` テーブルで持ち独立 note 行を発行しないため、
/// `createdNote` は announce id / `ap_id` + 元 note + renoter から
/// [`build_renote_miss_note`] で組み立てる。
#[allow(clippy::similar_names)] // renoter (= 行為者) / renoted (= 対象) は AP 用語
async fn handle_renote(state: &AppState, renote_id: Option<&str>) -> Response {
    let Some(rid) = renote_id else {
        return error_with_status(
            StatusCode::BAD_REQUEST,
            "INVALID_PARAM",
            "renoteId is required",
        );
    };
    let Ok(target_id) = rid.parse::<i64>() else {
        return error_with_status(
            StatusCode::BAD_REQUEST,
            "INVALID_PARAM",
            "renoteId is not a valid note id",
        );
    };

    // local_api の renote (= Announce) 経路を呼ぶ。Path(i64) で対象 note を渡す。
    let resp =
        local_api::renotes::create(State(state.clone()), axum::extract::Path(target_id)).await;
    let status = resp.status();
    let body_json = match collect_json(resp).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    if !status.is_success() {
        let message = body_json
            .get("error")
            .and_then(JsonValue::as_str)
            .unwrap_or("renote failed");
        // 404 = note 不在、400 = visibility 不可、422 = 自己 renote、503 = DB。
        let (code, st) = match status.as_u16() {
            404 => ("NO_SUCH_NOTE", StatusCode::NOT_FOUND),
            400 | 422 => ("CANNOT_RENOTE", StatusCode::BAD_REQUEST),
            503 => ("UNAVAILABLE", StatusCode::SERVICE_UNAVAILABLE),
            _ => ("INTERNAL_ERROR", StatusCode::INTERNAL_SERVER_ERROR),
        };
        return error_with_status(st, code, message);
    }

    // AnnounceResponse { id, ap_id, note_id, queued_deliveries }。
    let announce_id = body_json.get("id").and_then(JsonValue::as_i64).unwrap_or(0);
    let announce_ap_id = body_json
        .get("ap_id")
        .and_then(JsonValue::as_str)
        .unwrap_or_default()
        .to_string();

    // renote MissNote の合成に必要な要素を集める。
    let Some(viewer) = resolve_self_actor_id(state).await else {
        return error_with_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "local actor initialization failed",
        );
    };
    let Ok(Some(local_actor)) = sakurasato_core::repo::actor::get_by_id(state.pool(), viewer).await
    else {
        return error_with_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "local actor fetch failed",
        );
    };
    let Ok(Some(target_entry)) =
        repo::note::get_timeline_entry_by_id(state.pool(), target_id).await
    else {
        return error_with_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "renote target lookup failed",
        );
    };

    let host = &state.config().server.host;
    let summaries = bulk_load_note_summaries(state.pool(), &[target_entry.id], viewer).await;
    let summary = summaries.remove_summary(target_entry.id);
    let renoted = timeline_entry_to_miss_note(&target_entry, &summary, host, viewer);
    let renoter = from_actor_and_counts(&local_actor, 0, 0, 0);
    let created_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

    let created_note = build_renote_miss_note(
        announce_id,
        &announce_ap_id,
        &created_at,
        renoter,
        viewer,
        renoted,
    );
    (StatusCode::OK, Json(json!({ "createdNote": created_note }))).into_response()
}

/// Misskey body → 既存 `local_api::notes::CreateNoteRequest` に翻訳する。
///
/// `Err` の中身 (= `Response`) は大きいので Box する ── clippy の
/// `result_large_err` lint を満たすため。
#[allow(
    clippy::result_large_err,
    reason = "翻訳エラーは即座に return するので box する利得が小さい"
)]
fn translate_create_body(
    body: &CreateNoteBody,
    in_reply_to_ap_id: Option<String>,
) -> Result<local_api::notes::CreateNoteRequest, Response> {
    let text = body.text.clone().unwrap_or_default();
    if text.trim().is_empty() {
        return Err(error_with_status(
            StatusCode::BAD_REQUEST,
            "INVALID_PARAM",
            "text must not be empty",
        ));
    }

    // Misskey の visibility → 内部 visibility 文字列に変換 (= `MissNote.visibility`
    // と逆方向)。
    let internal_visibility = match body.visibility.as_deref() {
        None | Some("public") => Some("public".to_string()),
        Some("home") => Some("unlisted".to_string()),
        Some("followers") => Some("followers".to_string()),
        Some("specified") => Some("direct".to_string()),
        Some(other) => {
            return Err(error_with_status(
                StatusCode::BAD_REQUEST,
                "INVALID_PARAM",
                &format!("unknown visibility: {other:?}"),
            ));
        }
    };

    // `in_reply_to_ap_id` は呼び出し側 (= `create()`) が `replyId` → 親 note の
    // `ap_id` に解決済み (= async な note lookup を handler で済ませる)。

    // **PR #166 review item 3**: parse 失敗を silent drop せず `400 INVALID_PARAM`
    // で弾く。黙って捨てると client は「添付付きで投稿した」つもりが添付無し
    // note になり、原因が分からない。空配列 (= 添付なし) は当然許可。
    let attachment_ids: Vec<i64> = body
        .file_ids
        .iter()
        .map(|s| {
            s.parse::<i64>().map_err(|_| {
                error_with_status(
                    StatusCode::BAD_REQUEST,
                    "INVALID_PARAM",
                    "fileIds contains a non-numeric id",
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(local_api::notes::CreateNoteRequest {
        content: text,
        summary: body.cw.clone(),
        visibility: internal_visibility,
        sensitive: None,
        language: None,
        in_reply_to_ap_id,
        attachment_ids,
    })
}

/// `local_api::notes::create` の error response (= `{"error": "..."}`) を
/// Misskey 互換 (= `{"error": {"code", "message"}}`) に翻訳する。
///
/// **PR #166 review item 4**: 唯一の呼び出し元は [`create`]。`local_api::notes::
/// create` はバリデーション失敗 (= 添付不在含む) を **400**、権限拒否を **403**、
/// サービス不能を **503** で返し、**404 は返さない** (= create に「note 不在」の
/// 概念が無い)。下記 `404 => NO_SUCH_NOTE` arm は防御的に残すが現状到達しない。
/// 将来 create 経路が「親 note / 添付が無い」で 404 を返すようになったら、その
/// 意味は `NO_SUCH_NOTE` ではないので call-site 固有のマッピングに分離すること。
fn translate_local_error_to_misskey(status: StatusCode, body: &JsonValue) -> Response {
    let message = body
        .get("error")
        .and_then(JsonValue::as_str)
        .unwrap_or("operation failed");
    let code = match status.as_u16() {
        400 => "INVALID_PARAM",
        403 => "PERMISSION_DENIED",
        404 => "NO_SUCH_NOTE",
        409 => "CONFLICT",
        503 => "UNAVAILABLE",
        _ => "INTERNAL_ERROR",
    };
    error_with_status(status, code, message)
}

/// Delete activity を組み立てる。`object` には note の `ap_id` を URI 参照で
/// 載せる ── inline 埋め込みは「相手が既に削除済」のケースで重い無駄になる。
fn build_delete_note_activity(
    actor: &sakurasato_core::model::ActorRow,
    note: &sakurasato_core::model::NoteRow,
) -> JsonValue {
    let now = chrono::Utc::now();
    // **PR #166 review item 2**: activity id は `note.id` (BIGSERIAL、再利用
    // されない) で決定論的に組む。ms timestamp 形式だと Delete 配送の retry や
    // 同一 note への再呼び出しで毎回違う id になり、受信側で重複適用され得る。
    // note 本体の Create wrapper (`{note_ap_id}/activity`) と同じく **note-anchored**
    // な安定 id に揃える ── これで retry や再削除でも id が一定し、受信側の
    // dedup が効く。`!note.is_local` は呼び出し前 (delete handler) で保証済みなので
    // base URL は常に自ドメイン。
    let activity_id = format!(
        "{ap_id}/activity/delete-{id}",
        ap_id = note.ap_id,
        id = note.id,
    );
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": activity_id,
        "type": "Delete",
        "actor": actor.ap_id,
        "object": note.ap_id,
        "published": now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    })
}

/// axum Response の body を JSON として collect する。失敗時は 500 Response。
#[allow(
    clippy::result_large_err,
    reason = "fallback Response は即座に return するので box する利得が小さい"
)]
async fn collect_json(resp: Response) -> Result<JsonValue, Response> {
    let bytes = match resp.into_body().collect().await {
        Ok(b) => b.to_bytes(),
        Err(err) => {
            tracing::error!(?err, "collect_json: body collect failed");
            return Err(error_with_status(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "response parsing failed",
            ));
        }
    };
    if bytes.is_empty() {
        return Ok(JsonValue::Null);
    }
    serde_json::from_slice(&bytes).map_err(|err| {
        tracing::error!(?err, "collect_json: JSON parse failed");
        error_with_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "response parsing failed",
        )
    })
}

/// `bulk_load_note_summaries` の戻り型 `HashMap<i64, NoteSummary>` に対する
/// 便利な extractor。`remove_summary` で id ごとに取り出して owned で使う。
trait SummariesExt {
    fn remove_summary(self, note_id: i64) -> NoteSummary;
}

impl SummariesExt for std::collections::HashMap<i64, NoteSummary> {
    fn remove_summary(mut self, note_id: i64) -> NoteSummary {
        self.remove(&note_id).unwrap_or(NoteSummary {
            reactions: Vec::new(),
            announce: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_id_opt_handles_missing_and_invalid() {
        assert_eq!(parse_id_opt(None), None);
        assert_eq!(parse_id_opt(Some("")), None);
        assert_eq!(parse_id_opt(Some("abc")), None);
        assert_eq!(parse_id_opt(Some("42")), Some(42));
        assert_eq!(parse_id_opt(Some("-1")), Some(-1));
    }

    #[test]
    fn ms_epoch_round_trip() {
        let dt = ms_epoch_to_datetime(1_700_000_000_000).expect("valid epoch ms");
        assert_eq!(dt.timestamp_millis(), 1_700_000_000_000);
    }

    #[test]
    fn ms_epoch_rejects_extremes() {
        // i64::MAX ms はまだ chrono で扱える範囲なので valid。極端な負値だけ確認。
        assert!(ms_epoch_to_datetime(0).is_some());
        // 巨大値: chrono の上限を超えるなら None。
        let out_of_range = i64::MAX;
        // どちらか (Some/None) で OK ── 仕様上 panic しないことだけ確認。
        let _ = ms_epoch_to_datetime(out_of_range);
    }
}
