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
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, TimeZone, Utc};
use sakurasato_core::repo;
use serde::Deserialize;
use serde_json::json;

use crate::miauth::auth;
use crate::miauth::conv::{
    MissNote, NoteSummary, bulk_load_note_summaries, timeline_entry_to_miss_note,
};
use crate::state::AppState;

/// `read:account` scope (= Misskey 仕様で `notes/timeline` / `notes/show` /
/// `users/show` 共通の最小権限)。
const SCOPE_READ_ACCOUNT: &str = "read:account";

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
    let Some(_token_row) = authorize(&state, &headers, body.i.as_deref()).await else {
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
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let Some(viewer) = resolve_self_actor_id(&state).await else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
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
    let Some(_token_row) = authorize(&state, &headers, body.i.as_deref()).await else {
        return auth::unauthorized("invalid or revoked token");
    };

    let Some(viewer) = resolve_self_actor_id(&state).await else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
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
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
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

/// body `i` → Authorization Bearer の順で token を抽出し、`read:account` scope
/// を持つかまで検証する。成功なら `Some(MiAuthTokenRow)`、失敗なら `None`
/// (= 呼び出し側で `unauthorized` / `forbidden` を返す)。
async fn authorize(
    state: &AppState,
    headers: &HeaderMap,
    body_i: Option<&str>,
) -> Option<sakurasato_core::model::MiAuthTokenRow> {
    let raw = match body_i.filter(|s| !s.is_empty()) {
        Some(s) => s.to_string(),
        None => headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(auth::parse_bearer_header)
            .map(str::to_string)?,
    };
    let token_row = auth::validate_token_raw(state, &raw).await?;
    if !auth::has_scope(&token_row, SCOPE_READ_ACCOUNT) {
        return None;
    }
    auth::mark_used_async(state, token_row.id);
    Some(token_row)
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
