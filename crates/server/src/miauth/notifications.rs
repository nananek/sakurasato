//! `POST /api/i/notifications` / `POST /api/notifications/mark-all-as-read`
//! (= 通知 #206 PR2)。
//!
//! in-app 通知フィード (migration 0020 `notification`) を **Misskey の Notification
//! wire 形** で返し、既読化する。Aria の通知タブが叩く経路。
//!
//! ## wire 仕様 (clean-room)
//!
//! - <https://api-doc.misskey.io/api/endpoints/i/notifications>
//! - <https://api-doc.misskey.io/api/endpoints/notifications/mark-all-as-read>
//!
//! `i/notifications` の `markAsRead` は **default `true`** で、Misskey では「一覧
//! 取得そのものが既読化操作」になる (Aria はこれでベルをクリアする)。`list`
//! はこれを honor し、`markAsRead: false` を明示したときだけ既読化を抑止する。
//!
//! Notification object (本実装が返すサブセット):
//!
//! ```json
//! { "id": "...", "createdAt": "...", "type": "reaction"|"follow"|"mention"|
//!   "renote"|"quote"|"receiveFollowRequest", "isRead": false,
//!   "userId": "...", "user": MissUser,  // notifier
//!   "note": MissNote,                    // note 系のみ
//!   "reaction": "👍" }                   // reaction のみ
//! ```
//!
//! Sakurasato の `NotificationEvent` → Misskey `type` マッピング:
//! - `reaction` → `reaction` / `renote` → `renote` / `quote` → `quote`
//! - `mention` / `direct` → `mention` (Misskey に DM 専用 type は無い)
//! - `follow` → `follow` / `follow_request` → `receiveFollowRequest`
//!
//! AGPL discipline は [[agpl-discipline-miauth]] / [`crate::miauth`] module doc。

use std::collections::HashMap;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use sakurasato_core::model::NotificationRow;
use sakurasato_core::repo;
use serde::Deserialize;
use serde_json::{Value as JsonValue, json};

use crate::miauth::auth;
use crate::miauth::conv::{
    NoteSummary, bulk_load_note_summaries, from_actor_and_counts, timeline_entry_to_miss_note,
};
use crate::miauth::error::error_resp;
use crate::state::AppState;

/// 読み取り権限。`/api/i` と同じく `read:account` を要求する (= 通知は account
/// scope の一部。お一人様サーバなので token = 当該 user に閉じる)。
const SCOPE_READ_ACCOUNT: &str = "read:account";

const LIMIT_DEFAULT: i64 = 20;
const LIMIT_MAX: i64 = 100;

#[derive(Debug, Deserialize, Default)]
pub struct NotificationsBody {
    #[serde(default)]
    pub i: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(rename = "sinceId", default)]
    pub since_id: Option<String>,
    #[serde(rename = "untilId", default)]
    pub until_id: Option<String>,
    /// Misskey 互換: `markAsRead` (**default `true`**)。取得そのものを既読化操作と
    /// する仕様で、Aria はこれを叩いてベルの未読をクリアする。`None` (未指定) は
    /// `true` 扱い。`Some(false)` を明示したときだけ既読化を抑止する。
    #[serde(rename = "markAsRead", default)]
    pub mark_as_read: Option<bool>,
}

#[derive(Debug, Deserialize, Default)]
pub struct MarkAllBody {
    #[serde(default)]
    pub i: Option<String>,
}

/// `POST /api/i/notifications` handler ── 通知一覧 (`id DESC`, sinceId/untilId 排他)。
pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<NotificationsBody>>,
) -> Response {
    let body = body.map(|j| j.0).unwrap_or_default();
    let Some(_token) =
        auth::require_scope(&state, &headers, body.i.as_deref(), SCOPE_READ_ACCOUNT).await
    else {
        return auth::unauthorized("invalid or revoked token");
    };
    let Some(recipient) = resolve_self_actor_id(&state).await else {
        return error_resp(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "local actor initialization failed",
        );
    };

    let limit = body.limit.unwrap_or(LIMIT_DEFAULT).clamp(1, LIMIT_MAX);
    let since_id = parse_id_opt(body.since_id.as_deref());
    let until_id = parse_id_opt(body.until_id.as_deref());

    let rows =
        match repo::notification::list(state.pool(), recipient, limit, since_id, until_id).await {
            Ok(v) => v,
            Err(err) => {
                tracing::error!(?err, "miauth i/notifications: list failed");
                return error_resp(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    "notification query failed",
                );
            }
        };

    // 対象 note の reaction / announce 集計を 1 度に bulk load する (= N+1 抑止)。
    let note_ids: Vec<i64> = rows.iter().filter_map(|r| r.note_id).collect();
    let summaries = bulk_load_note_summaries(state.pool(), &note_ids, recipient).await;
    let host = state.config().server.host.clone();

    // 同一 notifier の MissUser は使い回す (= reaction 連投で actor を引き直さない)。
    let mut actor_cache: HashMap<i64, JsonValue> = HashMap::new();
    let mut out: Vec<JsonValue> = Vec::with_capacity(rows.len());
    for row in &rows {
        out.push(build_notification(&state, row, &host, &summaries, &mut actor_cache).await);
    }

    // Misskey 互換: `i/notifications` は `markAsRead` (default true) で「取得 =
    // 既読化」する。これを実装しないと `count_unread` が減らず `/api/i` の
    // `hasUnreadNotification` が永遠に true のまま残り、Aria の通知ベルに新着
    // マークが空振りで付き続ける (報告バグ)。`markAsRead: false` を明示した
    // ときだけ抑止する。`out` 構築後に既読化するので返却ペイロードの `isRead` は
    // 取得時点の値 (= 通常 false) を保つ ── Misskey も「今回の新着」を見せてから
    // 既読化する。best-effort (失敗は warn のみで一覧は返す)。
    if body.mark_as_read != Some(false)
        && let Err(err) = repo::notification::mark_all_read(state.pool(), recipient).await
    {
        tracing::warn!(
            ?err,
            "miauth i/notifications: markAsRead mark_all_read failed"
        );
    }

    Json(out).into_response()
}

/// `POST /api/notifications/mark-all-as-read` handler ── 全件既読化、204 を返す。
pub async fn mark_all_read(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<MarkAllBody>>,
) -> Response {
    let body = body.map(|j| j.0).unwrap_or_default();
    let Some(_token) =
        auth::require_scope(&state, &headers, body.i.as_deref(), SCOPE_READ_ACCOUNT).await
    else {
        return auth::unauthorized("invalid or revoked token");
    };
    let Some(recipient) = resolve_self_actor_id(&state).await else {
        return error_resp(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "local actor initialization failed",
        );
    };
    if let Err(err) = repo::notification::mark_all_read(state.pool(), recipient).await {
        tracing::error!(?err, "miauth notifications/mark-all-as-read failed");
        return error_resp(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "mark all read failed",
        );
    }
    (StatusCode::NO_CONTENT, ()).into_response()
}

/// `NotificationRow` 1 行を Misskey Notification object に組み立てる。
async fn build_notification(
    state: &AppState,
    row: &NotificationRow,
    host: &str,
    summaries: &HashMap<i64, NoteSummary>,
    actor_cache: &mut HashMap<i64, JsonValue>,
) -> JsonValue {
    let mut obj = json!({
        "id": row.id.to_string(),
        "createdAt": row.created_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "type": map_event_type(&row.event_type),
        "isRead": row.is_read,
    });

    // notifier (= 通知を起こした相手)。
    if let Some(notifier_id) = row.notifier_actor_id {
        let user = if let Some(cached) = actor_cache.get(&notifier_id) {
            cached.clone()
        } else {
            let built = match repo::actor::get_by_id(state.pool(), notifier_id).await {
                Ok(Some(actor)) => serde_json::to_value(from_actor_and_counts(&actor, 0, 0, 0))
                    .unwrap_or(JsonValue::Null),
                _ => JsonValue::Null,
            };
            actor_cache.insert(notifier_id, built.clone());
            built
        };
        if !user.is_null() {
            obj["userId"] = json!(notifier_id.to_string());
            obj["user"] = user;
        }
    }

    // 対象 note (reaction / renote / quote / mention / direct)。
    if let Some(note_id) = row.note_id
        && let Ok(Some(entry)) = repo::note::get_timeline_entry_by_id(state.pool(), note_id).await
    {
        let empty = NoteSummary {
            reactions: Vec::new(),
            announce: None,
            my_reaction: None,
        };
        let summary = summaries.get(&note_id).unwrap_or(&empty);
        let note = timeline_entry_to_miss_note(&entry, summary, host);
        obj["note"] = serde_json::to_value(&note).unwrap_or(JsonValue::Null);
    }

    // reaction の内容。
    if let Some(reaction) = &row.reaction {
        obj["reaction"] = json!(reaction);
    }

    obj
}

/// `NotificationEvent::as_str()` の値 → Misskey Notification `type`。
fn map_event_type(event_type: &str) -> &'static str {
    match event_type {
        "reaction" => "reaction",
        "renote" => "renote",
        "quote" => "quote",
        "follow" => "follow",
        "follow_request" => "receiveFollowRequest",
        // mention / direct は Misskey に DM 専用 type が無いので "mention" に倒す。
        // 未知値も安全側で "mention" (= client が描画できる汎用通知)。
        _ => "mention",
    }
}

/// local actor (= recipient) の id を引く。お一人様サーバ前提で 1 件。
async fn resolve_self_actor_id(state: &AppState) -> Option<i64> {
    let host = &state.config().server.host;
    let user = &state.config().server.user;
    match repo::actor::get_by_username_host(state.pool(), user, host).await {
        Ok(Some(row)) if row.is_local => Some(row.id),
        _ => None,
    }
}

/// Misskey は id を **string** で渡すので i64 に parse。失敗は `None` (= 無視)。
fn parse_id_opt(s: Option<&str>) -> Option<i64> {
    s.and_then(|t| t.parse::<i64>().ok())
}
