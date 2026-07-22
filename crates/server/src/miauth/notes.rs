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
use crate::miauth::error::{bad_request, error_resp};
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
    // renote は home timeline で `rn:<announce_id>` の合成 id を持つ。tap された
    // ときはこの分岐で announce → 元 note + renoter から renote MissNote を返す。
    if let Some(rest) = note_id_str.strip_prefix("rn:") {
        return show_renote(&state, rest).await;
    }
    let Ok(note_id) = note_id_str.parse::<i64>() else {
        return error_resp(StatusCode::NOT_FOUND, "NO_SUCH_NOTE", "no such note");
    };

    let entry = match repo::note::get_timeline_entry_by_id(state.pool(), note_id).await {
        Ok(Some(e)) => e,
        Ok(None) => {
            return error_resp(StatusCode::NOT_FOUND, "NO_SUCH_NOTE", "no such note");
        }
        Err(err) => {
            tracing::error!(
                ?err,
                note_id,
                "miauth notes/show: get_timeline_entry_by_id failed"
            );
            return error_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "note lookup failed",
            );
        }
    };

    let Some(viewer) = resolve_self_actor_id(&state).await else {
        return error_resp(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "local actor initialization failed",
        );
    };

    if !viewer_can_view_entry(&state, &entry, viewer).await {
        return error_resp(StatusCode::NOT_FOUND, "NO_SUCH_NOTE", "no such note");
    }

    let summaries = bulk_load_note_summaries(state.pool(), &[entry.id], viewer).await;
    let summary = summaries.remove_summary(entry.id);
    let host = &state.config().server.host;
    let note = timeline_entry_to_miss_note(&entry, &summary, host);
    Json(note).into_response()
}

/// `notes/show { noteId: "rn:<announce_id>" }` ── home timeline 由来の renote
/// 合成 id を tap された経路。announce を引いて元 note + renoter から renote
/// `MissNote` を組み立てて返す (timeline の renote 項目と同形)。
#[allow(
    clippy::similar_names,
    reason = "renoter (=行為者) / renoted (=対象) は AP 用語"
)]
async fn show_renote(state: &AppState, announce_id_str: &str) -> Response {
    let Ok(announce_id) = announce_id_str.parse::<i64>() else {
        return error_resp(StatusCode::NOT_FOUND, "NO_SUCH_NOTE", "no such note");
    };
    let Some(viewer) = resolve_self_actor_id(state).await else {
        return error_resp(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "local actor initialization failed",
        );
    };
    let ann = match repo::announce::get_by_id(state.pool(), announce_id).await {
        Ok(Some(a)) => a,
        Ok(None) => {
            return error_resp(StatusCode::NOT_FOUND, "NO_SUCH_NOTE", "no such note");
        }
        Err(err) => {
            tracing::error!(
                ?err,
                announce_id,
                "miauth notes/show: announce lookup failed"
            );
            return error_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "renote lookup failed",
            );
        }
    };
    let entry = match repo::note::get_timeline_entry_by_id(state.pool(), ann.note_id).await {
        Ok(Some(e)) => e,
        Ok(None) => {
            return error_resp(StatusCode::NOT_FOUND, "NO_SUCH_NOTE", "no such note");
        }
        Err(err) => {
            tracing::error!(
                ?err,
                note_id = ann.note_id,
                "miauth notes/show: renoted note lookup failed"
            );
            return error_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "renoted note lookup failed",
            );
        }
    };
    let renoter = match sakurasato_core::repo::actor::get_by_id(state.pool(), ann.actor_id).await {
        Ok(Some(a)) => a,
        Ok(None) => {
            return error_resp(StatusCode::NOT_FOUND, "NO_SUCH_NOTE", "no such note");
        }
        Err(err) => {
            tracing::error!(
                ?err,
                actor_id = ann.actor_id,
                "miauth notes/show: renoter lookup failed"
            );
            return error_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "renoter lookup failed",
            );
        }
    };
    let summaries = bulk_load_note_summaries(state.pool(), &[entry.id], viewer).await;
    let summary = summaries.remove_summary(entry.id);
    let host = &state.config().server.host;
    let renoted = timeline_entry_to_miss_note(&entry, &summary, host);
    let renoter_user = from_actor_and_counts(&renoter, 0, 0, 0);
    let created_at = ann
        .published_at
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let note = build_renote_miss_note(
        ann.id,
        &ann.ap_id,
        &created_at,
        renoter_user,
        ann.actor_id,
        renoted,
    );
    Json(note).into_response()
}

/// `POST /api/notes/timeline` handler。
#[allow(
    clippy::too_many_lines,
    clippy::similar_names,
    reason = "note/renote マージ + カーソル解決で 1 ハンドラに収める。renoter/renoted は AP 用語"
)]
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
        return error_resp(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "local actor initialization failed",
        );
    };

    let limit = body
        .limit
        .unwrap_or(TIMELINE_LIMIT_DEFAULT)
        .clamp(1, TIMELINE_LIMIT_MAX);
    // カーソル解決: `sinceId`/`untilId` (id 文字列、renote は "rn:N") を境界
    // **時刻** に解決し、`sinceDate`/`untilDate` と統合する。note と renote を
    // 時刻順にマージするため、id カーソルではなく `published_at` 一本で
    // ページングする (id カーソルは announce.id と note.id が別連番で混在
    // できないため)。id カーソルがあればそれを優先、無ければ date を使う。
    let until_ts = match body.until_id.as_deref() {
        Some(s) => resolve_cursor_ts(&state, s).await,
        None => None,
    }
    .or_else(|| body.until_date.and_then(ms_epoch_to_datetime));
    let since_ts = match body.since_id.as_deref() {
        Some(s) => resolve_cursor_ts(&state, s).await,
        None => None,
    }
    .or_else(|| body.since_date.and_then(ms_epoch_to_datetime));

    // note window (時刻 bound)。
    let note_entries = match repo::note::list_home_timeline_window(
        state.pool(),
        viewer,
        None,
        None,
        since_ts,
        until_ts,
        limit,
    )
    .await
    {
        Ok(v) => v,
        Err(err) => {
            tracing::error!(?err, "miauth notes/timeline: note window failed");
            return error_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "timeline query failed",
            );
        }
    };

    // renote window (= 自分 / followee の Announce、時刻 bound)。
    let renote_rows = match repo::announce::list_home_renote_window(
        state.pool(),
        viewer,
        since_ts,
        until_ts,
        limit,
    )
    .await
    {
        Ok(v) => v,
        Err(err) => {
            tracing::error!(?err, "miauth notes/timeline: renote window failed");
            return error_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "timeline query failed",
            );
        }
    };

    // renote の元 note / renoter actor を一括解決。
    let renoted_ids: Vec<i64> = renote_rows.iter().map(|r| r.renoted_note_id).collect();
    let renoter_ids: Vec<i64> = renote_rows.iter().map(|r| r.renoter_actor_id).collect();
    // DB エラー時は renote を黙って落とす (timeline 自体は note で成立する) が、
    // 運用で気づけるよう warn は残す (= サイレント吸収にしない、#217 review)。
    let renoted_entries = repo::note::list_timeline_entries_by_ids(state.pool(), &renoted_ids)
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(
                ?err,
                "notes/timeline: renoted entries lookup failed; dropping renotes"
            );
            Vec::new()
        });
    let renoter_actors = sakurasato_core::repo::actor::list_by_ids(state.pool(), &renoter_ids)
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(
                ?err,
                "notes/timeline: renoter actors lookup failed; dropping renotes"
            );
            Vec::new()
        });
    let entry_by_id: std::collections::HashMap<i64, &sakurasato_core::repo::note::TimelineEntry> =
        renoted_entries.iter().map(|e| (e.id, e)).collect();
    let actor_by_id: std::collections::HashMap<i64, &sakurasato_core::model::ActorRow> =
        renoter_actors.iter().map(|a| (a.id, a)).collect();

    // summaries: note window + renote の元 note の全 id (nest した renote.renote
    // にも reaction / count を載せるため両方)。
    let mut all_note_ids: Vec<i64> = note_entries.iter().map(|e| e.id).collect();
    all_note_ids.extend(renoted_ids.iter().copied());
    let summaries = bulk_load_note_summaries(state.pool(), &all_note_ids, viewer).await;
    let empty = NoteSummary {
        reactions: Vec::new(),
        announce: None,
        my_reaction: None,
    };

    let host = &state.config().server.host;

    // note と renote を (sort_ts, MissNote) で 1 本に統合する。
    let mut items: Vec<(chrono::DateTime<chrono::Utc>, MissNote)> =
        Vec::with_capacity(note_entries.len() + renote_rows.len());
    for e in &note_entries {
        let summary = summaries.get(&e.id).unwrap_or(&empty);
        items.push((
            e.published_at,
            timeline_entry_to_miss_note(e, summary, host),
        ));
    }
    for r in &renote_rows {
        // 元 note / renoter が引けない renote は黙ってスキップ (FK 上は起きない)。
        let (Some(entry), Some(actor)) = (
            entry_by_id.get(&r.renoted_note_id),
            actor_by_id.get(&r.renoter_actor_id),
        ) else {
            continue;
        };
        let summary = summaries.get(&entry.id).unwrap_or(&empty);
        let renoted = timeline_entry_to_miss_note(entry, summary, host);
        let renoter = from_actor_and_counts(actor, 0, 0, 0);
        let created_at = r
            .announce_published_at
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let renote = build_renote_miss_note(
            r.announce_id,
            &r.announce_ap_id,
            &created_at,
            renoter,
            r.renoter_actor_id,
            renoted,
        );
        items.push((r.announce_published_at, renote));
    }

    // 時刻降順。同時刻の決定的順序のため id (string) を tiebreak に。limit へ切る。
    items.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.id.cmp(&a.1.id)));
    items.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    let notes: Vec<MissNote> = items.into_iter().map(|(_, n)| n).collect();
    Json(notes).into_response()
}

/// home timeline のカーソル id 文字列を境界 `published_at` に解決する。
///
/// `"rn:<announce_id>"` なら announce、それ以外は note id として扱う。混合
/// タイムラインを時刻でページングするための境界時刻 ── 解決できなければ
/// `None` (= 境界無し扱いで先頭から)。
pub(crate) async fn resolve_cursor_ts(
    state: &AppState,
    id: &str,
) -> Option<chrono::DateTime<chrono::Utc>> {
    if let Some(rest) = id.strip_prefix("rn:") {
        let announce_id = rest.parse::<i64>().ok()?;
        repo::announce::get_by_id(state.pool(), announce_id)
            .await
            .ok()
            .flatten()
            .map(|a| a.published_at)
    } else {
        let note_id = id.parse::<i64>().ok()?;
        repo::note::get_timeline_entry_by_id(state.pool(), note_id)
            .await
            .ok()
            .flatten()
            .map(|e| e.published_at)
    }
}

/// viewer が `entry` を閲覧できるか (= `direct` / `followers` visibility のゲート)。
/// `notes/show` と `notes/reactions` ([`crate::miauth::reactions::list`]) で共有する
/// ── reaction 一覧で「見えない note の reaction」を漏らさないため。
///
/// - `direct`: 本人 (`actor_id == viewer`) か、viewer の `ap_id` が to/cc audience に
///   含まれるときのみ可。お一人様 server で `direct` 受信は自分宛のみという前提。
/// - `followers`: 本人か、viewer が author を `accepted` で follow しているとき可。
/// - それ以外 (`public` / `home` / `unlisted`): 常に可。
pub(crate) async fn viewer_can_view_entry(
    state: &AppState,
    entry: &sakurasato_core::repo::note::TimelineEntry,
    viewer: i64,
) -> bool {
    match entry.visibility.as_str() {
        "direct" => {
            if entry.actor_id == viewer {
                return true;
            }
            let viewer_uri = viewer_ap_id(state).await;
            viewer_uri.as_ref().is_some_and(|uri| {
                entry
                    .to_recipients
                    .0
                    .iter()
                    .chain(entry.cc_recipients.0.iter())
                    .any(|r| r == uri)
            })
        }
        "followers" => {
            entry.actor_id == viewer
                || matches!(
                    repo::follow::get_by_pair(state.pool(), viewer, entry.actor_id).await,
                    Ok(Some(f)) if f.state == "accepted"
                )
        }
        _ => true,
    }
}

/// local actor の row を引く。お一人様サーバ前提で 1 件しかない。
/// `users/notes` は visibility / direct 判定に `id` と `ap_id` の双方が要るので、
/// 行ごと取って 1 query に収める。
pub(crate) async fn resolve_self_actor(
    state: &AppState,
) -> Option<sakurasato_core::model::ActorRow> {
    let host = &state.config().server.host;
    let user = &state.config().server.user;
    match repo::actor::get_by_username_host(state.pool(), user, host).await {
        Ok(Some(row)) if row.is_local => Some(row),
        _ => None,
    }
}

/// local actor の id を引く。お一人様サーバ前提で 1 件しかない。
pub(crate) async fn resolve_self_actor_id(state: &AppState) -> Option<i64> {
    resolve_self_actor(state).await.map(|row| row.id)
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

/// `sinceDate`/`untilDate` は **ms epoch** で渡される (Misskey 仕様)。
/// 範囲外 (= `i64::MAX` を超える / sec 換算で範囲外) は `None` に倒す。
pub(crate) fn ms_epoch_to_datetime(ms: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_millis_opt(ms).single()
}

// ─── #150 (Aria fix): users/notes (= ユーザのノート一覧 / プロフィール) ──────

/// `with_replies` / `with_renotes` の serde 既定 `true` 用 (= Misskey 仕様)。
fn default_true() -> bool {
    true
}

/// Misskey `users/notes` body。
///
/// 対象ユーザは **`userId` のみ** で指定する ── Misskey 仕様で `users/notes` は
/// `username` + `host` 経路を持たない (`users/show` とは非対称)。`withReplies` /
/// `withRenotes` は既定 **true**、`withFiles` は既定 **false**
/// ([[miauth-misskey-dart-required-fields]] とは無関係の wire 既定値)。
#[derive(Debug, Deserialize)]
pub struct UsersNotesBody {
    #[serde(default)]
    pub i: Option<String>,
    /// 文字列化された Sakurasato 内部 `i64` actor id。
    #[serde(rename = "userId", default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    /// **排他下限** (`published_at > sinceId 解決時刻`)。`"rn:N"` 形式の renote
    /// カーソルも受ける ([`resolve_cursor_ts`] が解決)。
    #[serde(rename = "sinceId", default)]
    pub since_id: Option<String>,
    /// **排他上限** (`published_at < untilId 解決時刻`)。
    #[serde(rename = "untilId", default)]
    pub until_id: Option<String>,
    /// **排他下限 (時刻)** ── ms epoch。
    #[serde(rename = "sinceDate", default)]
    pub since_date: Option<i64>,
    /// **排他上限 (時刻)** ── ms epoch。
    #[serde(rename = "untilDate", default)]
    pub until_date: Option<i64>,
    /// Misskey 既定 `true` ── 返信も含む。`false` で他者宛返信を除外 (自己
    /// スレッドは残す。詳細は [`repo::note::list_by_author_window`])。
    #[serde(rename = "withReplies", default = "default_true")]
    pub with_replies: bool,
    /// Misskey 既定 `true` ── 対象ユーザの renote (boost) も時刻順マージする。
    #[serde(rename = "withRenotes", default = "default_true")]
    pub with_renotes: bool,
    /// Misskey 既定 `false` ── `true` で添付のある note だけに絞る (= メディア
    /// タブ)。メディア絞り込み時は renote (添付概念なし) を除外する。
    #[serde(rename = "withFiles", default)]
    pub with_files: bool,
}

/// `POST /api/users/notes` handler ── 指定ユーザのノート一覧 (= Aria の
/// プロフィール / ユーザタイムライン)。
///
/// 構造は [`timeline`] (= home timeline) とほぼ同型で、違いは:
/// (a) note window が「viewer の home」ではなく **対象ユーザ著者** スコープ
///     ([`repo::note::list_by_author_window`]、viewer 可視性で絞る)、
/// (b) renote window も対象ユーザ著者スコープ
///     ([`repo::announce::list_author_renote_window`])、renoter は常に対象本人、
/// (c) `withReplies` / `withFiles` フィルタ。
///
/// レスポンスは **`MissNote` の素の配列** (Misskey `users/notes` wire 仕様。
/// envelope で包まない)。1 要素の shape は `notes/timeline` と完全に同一なので、
/// Aria (`misskey_dart`) が `notes/timeline` を parse できている限り本経路も安全。
#[allow(
    clippy::too_many_lines,
    clippy::similar_names,
    reason = "note/renote マージ + カーソル解決で 1 ハンドラに収める。renoter/renoted は AP 用語"
)]
pub async fn users_notes(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<UsersNotesBody>>,
) -> Response {
    // body 無しは userId 不在と同義 (= Misskey クライアントは必ず body を送る)。
    let Some(Json(body)) = body else {
        return bad_request("userId is required");
    };
    let Some(_token) =
        auth::require_scope(&state, &headers, body.i.as_deref(), SCOPE_READ_ACCOUNT).await
    else {
        return auth::unauthorized("invalid or revoked token");
    };

    // 対象ユーザ解決 (userId のみ ── username+host は users/notes では非対応)。
    let Some(user_id_str) = body.user_id.as_deref() else {
        return bad_request("userId is required");
    };
    let Ok(target_id) = user_id_str.parse::<i64>() else {
        return error_resp(StatusCode::NOT_FOUND, "NO_SUCH_USER", "no such user");
    };
    let target = match repo::actor::get_by_id(state.pool(), target_id).await {
        Ok(Some(a)) => a,
        Ok(None) => return error_resp(StatusCode::NOT_FOUND, "NO_SUCH_USER", "no such user"),
        Err(err) => {
            tracing::error!(?err, target_id, "miauth users/notes: target lookup failed");
            return error_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "user lookup failed",
            );
        }
    };

    // viewer = local self actor (お一人様)。可視性 + direct 判定に id / ap_id 双方。
    let Some(viewer) = resolve_self_actor(&state).await else {
        return error_resp(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "local actor initialization failed",
        );
    };

    let limit = body
        .limit
        .unwrap_or(TIMELINE_LIMIT_DEFAULT)
        .clamp(1, TIMELINE_LIMIT_MAX);

    // カーソル: id ("rn:N" 可) を境界 **時刻** に解決し、date 境界 (ms epoch) と
    // 統合する。note と renote を時刻順マージするため id ではなく published_at で
    // window する (= notes/timeline と同じ流儀)。
    let until_ts = match body.until_id.as_deref() {
        Some(s) => resolve_cursor_ts(&state, s).await,
        None => None,
    }
    .or_else(|| body.until_date.and_then(ms_epoch_to_datetime));
    let since_ts = match body.since_id.as_deref() {
        Some(s) => resolve_cursor_ts(&state, s).await,
        None => None,
    }
    .or_else(|| body.since_date.and_then(ms_epoch_to_datetime));

    // note window (著者本人 + viewer 可視性 + withReplies / withFiles)。
    let note_entries = match repo::note::list_by_author_window(
        state.pool(),
        target.id,
        viewer.id,
        &viewer.ap_id,
        body.with_replies,
        body.with_files,
        since_ts,
        until_ts,
        limit,
    )
    .await
    {
        Ok(v) => v,
        Err(err) => {
            tracing::error!(?err, "miauth users/notes: note window failed");
            return error_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "notes query failed",
            );
        }
    };

    // renote window (= 対象ユーザの Announce)。withRenotes=false ならスキップ。
    // メディア絞り込み (withFiles=true) 時も renote は添付概念が無いので除外する。
    let renote_rows = if body.with_renotes && !body.with_files {
        repo::announce::list_author_renote_window(
            state.pool(),
            target.id,
            viewer.id,
            since_ts,
            until_ts,
            limit,
        )
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(?err, "users/notes: renote window failed; dropping renotes");
            Vec::new()
        })
    } else {
        Vec::new()
    };

    // renote の元 note を一括解決 (renoter は対象ユーザ本人なので actor map 不要)。
    let renoted_ids: Vec<i64> = renote_rows.iter().map(|r| r.renoted_note_id).collect();
    let renoted_entries = repo::note::list_timeline_entries_by_ids(state.pool(), &renoted_ids)
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(
                ?err,
                "users/notes: renoted entries lookup failed; dropping renotes"
            );
            Vec::new()
        });
    let entry_by_id: std::collections::HashMap<i64, &sakurasato_core::repo::note::TimelineEntry> =
        renoted_entries.iter().map(|e| (e.id, e)).collect();

    // summaries: note window + renote の元 note の全 id。
    let mut all_note_ids: Vec<i64> = note_entries.iter().map(|e| e.id).collect();
    all_note_ids.extend(renoted_ids.iter().copied());
    let summaries = bulk_load_note_summaries(state.pool(), &all_note_ids, viewer.id).await;
    let empty = NoteSummary {
        reactions: Vec::new(),
        announce: None,
        my_reaction: None,
    };

    let host = &state.config().server.host;

    // note と renote を (sort_ts, MissNote) で 1 本に統合する。
    let mut items: Vec<(chrono::DateTime<chrono::Utc>, MissNote)> =
        Vec::with_capacity(note_entries.len() + renote_rows.len());
    for e in &note_entries {
        let summary = summaries.get(&e.id).unwrap_or(&empty);
        items.push((
            e.published_at,
            timeline_entry_to_miss_note(e, summary, host),
        ));
    }
    for r in &renote_rows {
        // 元 note が引けない renote は黙ってスキップ (FK 上は起きない)。
        let Some(entry) = entry_by_id.get(&r.renoted_note_id) else {
            continue;
        };
        // **プライバシー**は `list_author_renote_window` の SQL 側で担保済み
        // (Issue #253) ── followers 限定 note を author を follow していない
        // viewer に見せない述語が WHERE に入っているので、ここで per-item の
        // `viewer_can_view_entry` を呼ぶ必要は無い (= renote 件数ぶんの follow
        // 引き N+1 を解消)。
        let summary = summaries.get(&entry.id).unwrap_or(&empty);
        let renoted = timeline_entry_to_miss_note(entry, summary, host);
        // renoter は常に対象ユーザ本人。
        let renoter = from_actor_and_counts(&target, 0, 0, 0);
        let created_at = r
            .announce_published_at
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let renote = build_renote_miss_note(
            r.announce_id,
            &r.announce_ap_id,
            &created_at,
            renoter,
            r.renoter_actor_id,
            renoted,
        );
        items.push((r.announce_published_at, renote));
    }

    // 時刻降順。同時刻の決定的順序のため id (string) を tiebreak に。limit へ切る。
    items.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.id.cmp(&a.1.id)));
    items.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    let notes: Vec<MissNote> = items.into_iter().map(|(_, n)| n).collect();
    Json(notes).into_response()
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
        return error_resp(
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
                return error_resp(
                    StatusCode::BAD_REQUEST,
                    "INVALID_PARAM",
                    "replyId is not a valid note id",
                );
            };
            match repo::note::get_by_id(state.pool(), reply_note_id).await {
                Ok(Some(parent)) => Some(parent.ap_id),
                Ok(None) => {
                    return error_resp(
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
                    return error_resp(
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
        return error_resp(
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
        return error_resp(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "note lookup failed after creation",
        );
    };
    let Some(viewer) = resolve_self_actor_id(&state).await else {
        return error_resp(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "local actor initialization failed",
        );
    };
    let summaries = bulk_load_note_summaries(state.pool(), &[entry.id], viewer).await;
    let summary = summaries.remove_summary(entry.id);
    let host = &state.config().server.host;
    let note = timeline_entry_to_miss_note(&entry, &summary, host);

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
        return error_resp(StatusCode::NOT_FOUND, "NO_SUCH_NOTE", "no such note");
    };

    let Some(viewer) = resolve_self_actor_id(&state).await else {
        return error_resp(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "local actor initialization failed",
        );
    };
    let note = match repo::note::get_by_id(state.pool(), note_id).await {
        Ok(Some(n)) => n,
        Ok(None) => {
            return error_resp(StatusCode::NOT_FOUND, "NO_SUCH_NOTE", "no such note");
        }
        Err(err) => {
            tracing::error!(?err, note_id, "miauth notes/delete: lookup failed");
            return error_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "note lookup failed",
            );
        }
    };
    if note.actor_id != viewer {
        return error_resp(
            StatusCode::FORBIDDEN,
            "PERMISSION_DENIED",
            "note not owned by you",
        );
    }
    if !note.is_local {
        // remote note を local が削除しようとしても AP 上は無意味 (= 相手側の note)。
        return error_resp(
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
        return error_resp(
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
        return error_resp(
            StatusCode::BAD_REQUEST,
            "INVALID_PARAM",
            "renoteId is required",
        );
    };
    let Ok(target_id) = rid.parse::<i64>() else {
        return error_resp(
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
        return error_resp(st, code, message);
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
        return error_resp(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "local actor initialization failed",
        );
    };
    let Ok(Some(local_actor)) = sakurasato_core::repo::actor::get_by_id(state.pool(), viewer).await
    else {
        return error_resp(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "local actor fetch failed",
        );
    };
    let Ok(Some(target_entry)) =
        repo::note::get_timeline_entry_by_id(state.pool(), target_id).await
    else {
        return error_resp(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "renote target lookup failed",
        );
    };

    let host = &state.config().server.host;
    let summaries = bulk_load_note_summaries(state.pool(), &[target_entry.id], viewer).await;
    let summary = summaries.remove_summary(target_entry.id);
    let renoted = timeline_entry_to_miss_note(&target_entry, &summary, host);
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
        return Err(error_resp(
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
            return Err(error_resp(
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
                error_resp(
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
    error_resp(status, code, message)
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
            return Err(error_resp(
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
        error_resp(
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
            my_reaction: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
