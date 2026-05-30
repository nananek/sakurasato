//! `PATCH /api/v1/actor/profile` — local actor のプロフィール編集 (M7)。
//!
//! TUI から `display_name` / `summary` / `icon_media_id` / `image_media_id`
//! のいずれかを更新する。フィールドは個別に `null` を渡せば削除、`undefined`
//! (= JSON で省略) なら据え置き。
//!
//! `icon_media_id` / `image_media_id` は `POST /api/v1/media?kind=avatar`
//! などで先にアップロードしておいた `media.id` を指す ── このハンドラ自体は
//! バイト列を扱わず、media 行から `storage_key` を引いて actor の
//! `icon_url` / `image_url` を `https://<host>/media/<key>` に書き換える。
//!
//! プロフィール更新後は **Update Activity** をフォロワー全員に配送する。
//! 配送先 inbox 集合は `repo::follow::list_accepted_inboxes` で取り、
//! `delivery::enqueue_activity` で 1 行ずつ enqueue する (note と同じ作法)。

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use sakurasato_core::model::ActorRow;
use sakurasato_core::repo;
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use tracing::{error, warn};

use crate::delivery;
use crate::local_api::media::build_media_url;
use crate::routes::actor::build_actor_json;
use crate::state::AppState;

const DISPLAY_NAME_MAX: usize = 100;
const SUMMARY_MAX: usize = 5_000;
const PUBLIC_URI: &str = "https://www.w3.org/ns/activitystreams#Public";

/// PATCH ボディ。
///
/// **2 段の Option**: 外側 `Option` は「フィールド省略」、内側 `Option<String>`
/// は「明示的に null = 削除」。`#[serde(default, deserialize_with = ...)]` で
/// 外側 / 内側を区別したいが、serde の素朴な振る舞いでは `null` も `None` に
/// なる ── そこで `serde_with::rust::double_option` 相当を自前で書く代わり
/// に、明示クリア専用フラグ (`clear_*`) を別に渡してもらう設計とする。
/// プロトコルが少し冗長になるが、依存を増やさない (`serde_with` は重め)。
#[derive(Debug, Deserialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "4 clear_* フラグは optional field の null 指示と等価。state machine 化するより明示の方が読める"
)]
pub struct ProfileUpdate {
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub clear_display_name: bool,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub clear_summary: bool,
    /// `media.id` of an `avatar` upload to set as the actor icon.
    #[serde(default)]
    pub icon_media_id: Option<i64>,
    #[serde(default)]
    pub clear_icon: bool,
    /// `media.id` of a `header` upload to set as the actor image (banner).
    #[serde(default)]
    pub image_media_id: Option<i64>,
    #[serde(default)]
    pub clear_image: bool,
}

#[derive(Debug, Serialize)]
pub struct ProfileResponse {
    pub ap_id: String,
    pub preferred_username: String,
    pub display_name: Option<String>,
    pub summary: Option<String>,
    pub icon_url: Option<String>,
    pub image_url: Option<String>,
    /// この更新で `delivery_queue` に積まれた行数。0 = フォロワー無し。
    pub queued_deliveries: usize,
}

pub async fn patch(State(state): State<AppState>, Json(req): Json<ProfileUpdate>) -> Response {
    if let Err(reason) = validate(&req) {
        return bad_request(reason);
    }

    let host = state.config().server.host.clone();
    let user = state.config().server.user.clone();
    let local_actor = match repo::actor::get_by_username_host(state.pool(), &user, &host).await {
        Ok(Some(row)) if row.is_local => row,
        Ok(_) => {
            return error_with_body(
                StatusCode::SERVICE_UNAVAILABLE,
                "local actor not initialized; run `sakurasato init`",
            );
        }
        Err(err) => {
            error!(?err, "PATCH profile: local actor lookup failed");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };

    // メディア id → URL に解決する。`clear_icon` / `clear_image` が立っている
    // 場合は明示的に Some(None) (= NULL を書く)、メディア id が指定されている
    // 場合は Some(Some(URL))、どちらも無ければ None (= 触らない) になる。
    let icon_url = match resolve_media_url(
        &state,
        &local_actor,
        req.icon_media_id,
        req.clear_icon,
        "avatar",
    )
    .await
    {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let image_url = match resolve_media_url(
        &state,
        &local_actor,
        req.image_media_id,
        req.clear_image,
        "header",
    )
    .await
    {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    let display_name = if req.clear_display_name {
        Some(None)
    } else {
        req.display_name.map(|s| {
            let trimmed = s.trim().to_string();
            (!trimmed.is_empty()).then_some(trimmed)
        })
    };
    let summary = if req.clear_summary {
        Some(None)
    } else {
        req.summary.map(|s| {
            let trimmed = s.trim().to_string();
            (!trimmed.is_empty()).then_some(trimmed)
        })
    };

    let updated = match repo::actor::update_profile(
        state.pool(),
        local_actor.id,
        display_name,
        summary,
        icon_url,
        image_url,
    )
    .await
    {
        Ok(row) => row,
        Err(err) => {
            error!(?err, "PATCH profile: update failed");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };

    let activity = build_update_activity(&updated);
    let queued = enqueue_to_followers(&state, &updated, &activity).await;

    let body = ProfileResponse {
        ap_id: updated.ap_id.clone(),
        preferred_username: updated.preferred_username.clone(),
        display_name: updated.display_name.clone(),
        summary: updated.summary.clone(),
        icon_url: updated.icon_url.clone(),
        image_url: updated.image_url.clone(),
        queued_deliveries: queued,
    };
    (StatusCode::OK, Json(body)).into_response()
}

fn validate(req: &ProfileUpdate) -> Result<(), &'static str> {
    if let Some(name) = req.display_name.as_ref()
        && name.chars().count() > DISPLAY_NAME_MAX
    {
        return Err("display_name exceeds the 100-character limit");
    }
    if let Some(summary) = req.summary.as_ref()
        && summary.chars().count() > SUMMARY_MAX
    {
        return Err("summary exceeds the 5000-character limit");
    }
    // clear_* と 値指定の同時送付は意図不明なので拒否する。
    if req.clear_display_name && req.display_name.is_some() {
        return Err("display_name and clear_display_name cannot be set together");
    }
    if req.clear_summary && req.summary.is_some() {
        return Err("summary and clear_summary cannot be set together");
    }
    if req.clear_icon && req.icon_media_id.is_some() {
        return Err("icon_media_id and clear_icon cannot be set together");
    }
    if req.clear_image && req.image_media_id.is_some() {
        return Err("image_media_id and clear_image cannot be set together");
    }
    Ok(())
}

/// `media_id` から `https://<host>/media/<key>` を組み立てる。
///
/// - `clear` が true → `Ok(Some(None))` (= NULL を書く)
/// - `media_id` が `Some` → 行を取得して所有者と kind を検証し
///   `Ok(Some(Some(url)))` を返す。
/// - どちらも無し → `Ok(None)` (= 触らない)
///
/// 検証:
/// - メディアが存在し、`owner_actor_id == local_actor.id`
/// - `kind == expected_kind` (= avatar / header)
async fn resolve_media_url(
    state: &AppState,
    local_actor: &ActorRow,
    media_id: Option<i64>,
    clear: bool,
    expected_kind: &str,
) -> Result<Option<Option<String>>, Response> {
    if clear {
        return Ok(Some(None));
    }
    let Some(id) = media_id else {
        return Ok(None);
    };
    let row = match repo::media::get_by_id(state.pool(), id).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            return Err(error_with_body(
                StatusCode::BAD_REQUEST,
                &format!("media id {id} not found"),
            ));
        }
        Err(err) => {
            error!(?err, media_id = id, "media lookup failed");
            return Err(StatusCode::SERVICE_UNAVAILABLE.into_response());
        }
    };
    if row.owner_actor_id != local_actor.id {
        warn!(
            media_id = id,
            owner = row.owner_actor_id,
            requester = local_actor.id,
            "PATCH profile: media owner mismatch"
        );
        return Err(error_with_body(
            StatusCode::FORBIDDEN,
            "media is not owned by the local actor",
        ));
    }
    if row.kind != expected_kind {
        return Err(error_with_body(
            StatusCode::BAD_REQUEST,
            &format!(
                "media id {id} has kind {:?}, expected {expected_kind:?}",
                row.kind
            ),
        ));
    }
    let url = build_media_url(&state.config().server.host, &row.storage_key);
    Ok(Some(Some(url)))
}

/// `Update` activity を組み立てる。`object` には更新後の actor JSON を埋め込む。
///
/// 配送方針: フォロワー全員に「自分の actor が変わった」と伝えるので、
/// `to = [Public]` / `cc = [followers]` (Mastodon の actor Update と同じ)。
pub(crate) fn build_update_activity(actor: &ActorRow) -> JsonValue {
    let actor_object = serde_json::to_value(build_actor_json(actor))
        .expect("ActorJson serializes to JSON without error");
    let now = chrono::Utc::now();
    let activity_id = format!(
        "{ap_id}#updates/{ts}",
        ap_id = actor.ap_id,
        ts = now.timestamp_millis(),
    );
    let followers = actor
        .followers_url
        .clone()
        .unwrap_or_else(|| format!("{}/followers", actor.ap_id));
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "type": "Update",
        "id": activity_id,
        "actor": actor.ap_id,
        "to": [PUBLIC_URI],
        "cc": [followers],
        "published": now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "object": actor_object,
    })
}

async fn enqueue_to_followers(
    state: &AppState,
    local_actor: &ActorRow,
    activity: &JsonValue,
) -> usize {
    let inboxes = match repo::follow::list_accepted_inboxes(state.pool(), local_actor.id).await {
        Ok(list) => list,
        Err(err) => {
            warn!(?err, "PATCH profile: list_accepted_inboxes failed");
            return 0;
        }
    };
    let mut queued = 0_usize;
    for inbox in &inboxes {
        match delivery::enqueue_activity(state.pool(), local_actor.id, inbox, activity).await {
            Ok(_row) => queued += 1,
            Err(err) => warn!(?err, %inbox, "PATCH profile: enqueue failed"),
        }
    }
    queued
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
    fn validate_rejects_overlapping_clear_and_value() {
        let req = ProfileUpdate {
            display_name: Some("x".into()),
            clear_display_name: true,
            summary: None,
            clear_summary: false,
            icon_media_id: None,
            clear_icon: false,
            image_media_id: None,
            clear_image: false,
        };
        assert!(validate(&req).is_err());
    }

    #[test]
    fn validate_rejects_too_long_summary() {
        let req = ProfileUpdate {
            display_name: None,
            clear_display_name: false,
            summary: Some("x".repeat(SUMMARY_MAX + 1)),
            clear_summary: false,
            icon_media_id: None,
            clear_icon: false,
            image_media_id: None,
            clear_image: false,
        };
        assert!(validate(&req).is_err());
    }

    #[test]
    fn validate_accepts_minimal_request() {
        let req = ProfileUpdate {
            display_name: Some("にゃー".into()),
            clear_display_name: false,
            summary: None,
            clear_summary: false,
            icon_media_id: None,
            clear_icon: false,
            image_media_id: None,
            clear_image: false,
        };
        assert!(validate(&req).is_ok());
    }
}
