//! `POST /api/v1/reactions` / `DELETE /api/v1/reactions/{id}` (M8 PR2)。
//!
//! ローカル user が自分の Note にリアクションを付けて連合先に通知する経路。
//!
//! ## 流れ (`POST`)
//!
//! 1. body の `note_id` を解決し、ローカルの `note` 行が見つかることを確認。
//!    （remote Note 受信は本 PR では未実装なので `note.is_local` 必須）
//! 2. `content` を検証:
//!    - 空 / 長すぎは 400。
//!    - `:foo:` 形式ならローカル emoji 行を引いて `emoji_id` を紐付け、
//!      AP `tag: [Emoji]` を組み立てる。
//!    - Unicode は `emoji_id` = NULL、`tag` 無し。
//! 3. reaction 行を idempotent に insert ([`repo::reaction::insert_or_get`])。
//! 4. `EmojiReact` (custom emoji) / `Like` (Unicode) Activity を組み立て、
//!    `repo::follow::list_accepted_inboxes` で配送先を取り `delivery_queue`
//!    に inbox ごと 1 行ずつ push。
//!
//! ## 流れ (`DELETE`)
//!
//! 1. `reaction_id` で行を引く ── ローカル actor 所有でなければ 403。
//! 2. `Undo` Activity を組み立て、followers の inbox に配送。
//! 3. ローカル DB からも `reaction` 行を `delete_by_ap_id` で即時削除。
//!    削除前の `ap_id` を Undo の `object` に乗せる。

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use sakurasato_core::model::{ActorRow, EmojiRow};
use sakurasato_core::repo;
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use tracing::{error, warn};

use crate::delivery;
use crate::local_api::media::build_media_url;
use crate::state::AppState;

/// `content` の最大文字数。inbound の `EmojiReact` 検査 (256) と同値。
const CONTENT_MAX: usize = 256;

#[derive(Debug, Deserialize)]
pub struct CreateReactionRequest {
    pub note_id: i64,
    pub content: String,
}

#[derive(Debug, Serialize)]
pub struct ReactionResponse {
    pub id: i64,
    pub ap_id: String,
    pub note_id: i64,
    pub content: String,
    pub emoji_id: Option<i64>,
    pub queued_deliveries: usize,
}

pub async fn create(
    State(state): State<AppState>,
    Json(req): Json<CreateReactionRequest>,
) -> Response {
    if let Err(reason) = validate_content(&req.content) {
        return bad_request(reason);
    }
    let local_actor = match resolve_local_actor(&state).await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let note = match repo::note::get_by_id(state.pool(), req.note_id).await {
        Ok(Some(n)) if n.is_local => n,
        Ok(Some(_)) => {
            return error_with_body(
                StatusCode::NOT_FOUND,
                "reactions to remote notes are not supported yet",
            );
        }
        Ok(None) => {
            return error_with_body(StatusCode::NOT_FOUND, "note not found");
        }
        Err(err) => {
            error!(?err, "POST /api/v1/reactions: note lookup failed");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };

    // `:foo:` → ローカル emoji を引く。Unicode はここで None になる。
    let emoji = match resolve_local_emoji(&state, &req.content).await {
        Ok(opt) => opt,
        Err(resp) => return resp,
    };

    // `reaction.ap_id` は決定論的に `reaction-<id>` で組み立てたい (= Undo の
    // 突き合わせやログ追跡が容易) ので、insert 前に sequence の nextval を
    // 引いて id を確保する。BIGSERIAL は cycle しないので衝突は起きない。
    let reaction_id = match next_reaction_id(&state).await {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let ap_id = format!(
        "https://{host}/users/{user}/activities/reaction-{reaction_id}",
        host = state.config().server.host,
        user = local_actor.preferred_username,
    );

    let inserted = match repo::reaction::insert_or_get(
        state.pool(),
        &ap_id,
        note.id,
        local_actor.id,
        &req.content,
        emoji.as_ref().map(|e| e.id),
    )
    .await
    {
        Ok(r) => r,
        Err(err) => {
            error!(?err, "POST /api/v1/reactions: insert_or_get failed");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };

    // 既存行 (= 同 (note, actor, content) で別 ap_id がすでにあった) を返した
    // 場合は連合通知を再送しない。冪等性を保つ。
    let queued = if inserted.ap_id == ap_id {
        let activity = build_reaction_activity(
            &state,
            &local_actor,
            &note.ap_id,
            &req.content,
            emoji.as_ref(),
            &inserted.ap_id,
        );
        enqueue_to_followers(&state, &local_actor, &activity).await
    } else {
        0
    };

    let body = ReactionResponse {
        id: inserted.id,
        ap_id: inserted.ap_id.clone(),
        note_id: inserted.note_id,
        content: inserted.content,
        emoji_id: inserted.emoji_id,
        queued_deliveries: queued,
    };
    let location = format!("/api/v1/reactions/{}", inserted.id);
    let mut response = (StatusCode::CREATED, Json(body)).into_response();
    if let Ok(hv) = HeaderValue::from_str(&location) {
        response
            .headers_mut()
            .insert(HeaderName::from_static("location"), hv);
    }
    response
}

pub async fn delete(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let local_actor = match resolve_local_actor(&state).await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let row = match fetch_reaction_by_id(&state, id).await {
        Ok(Some(r)) => r,
        Ok(None) => return error_with_body(StatusCode::NOT_FOUND, "reaction not found"),
        Err(resp) => return resp,
    };
    if row.actor_id != local_actor.id {
        return error_with_body(StatusCode::FORBIDDEN, "reaction not owned by local actor");
    }
    build_and_dispatch_delete(&state, &local_actor, row).await
}

async fn fetch_reaction_by_id(
    state: &AppState,
    id: i64,
) -> Result<Option<sakurasato_core::model::ReactionRow>, Response> {
    match sqlx::query_as!(
        sakurasato_core::model::ReactionRow,
        r#"
        SELECT id, ap_id, note_id, actor_id, content, emoji_id, created_at
        FROM reaction WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(state.pool())
    .await
    {
        Ok(o) => Ok(o),
        Err(err) => {
            error!(?err, reaction_id = id, "fetch_reaction_by_id failed");
            Err(StatusCode::SERVICE_UNAVAILABLE.into_response())
        }
    }
}

async fn build_and_dispatch_delete(
    state: &AppState,
    local_actor: &ActorRow,
    row: sakurasato_core::model::ReactionRow,
) -> Response {
    // 元の Like / EmojiReact Activity を構築して Undo の object に埋める。
    // 簡単のため URI 参照 ({object: "<URI>"}) で済ます ── Misskey / Mastodon
    // とも `object` を URI で受けるのが標準。
    let undo_id = format!(
        "https://{host}/users/{user}/activities/undo-reaction-{id}",
        host = state.config().server.host,
        user = local_actor.preferred_username,
        id = row.id,
    );
    let activity = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": undo_id,
        "type": "Undo",
        "actor": local_actor.ap_id,
        "object": row.ap_id,
    });
    let queued = enqueue_to_followers(state, local_actor, &activity).await;

    // 配送 enqueue が成功してから DB から行を消す。失敗しても自分側だけ消す
    // と「相手はまだ反応中、自分は消した」のズレが残るので、ベストエフォート。
    if let Err(err) = repo::reaction::delete_by_ap_id(state.pool(), &row.ap_id).await {
        warn!(
            ?err,
            reaction_id = row.id,
            "DELETE /api/v1/reactions: row delete failed (Undo already queued)"
        );
    }
    (
        StatusCode::OK,
        Json(json!({
            "deleted": row.id,
            "queued_deliveries": queued,
        })),
    )
        .into_response()
}

async fn enqueue_to_followers(
    state: &AppState,
    local_actor: &ActorRow,
    activity: &JsonValue,
) -> usize {
    let inboxes = match repo::follow::list_accepted_inboxes(state.pool(), local_actor.id).await {
        Ok(list) => list,
        Err(err) => {
            warn!(?err, "enqueue_to_followers: list_accepted_inboxes failed");
            return 0;
        }
    };
    let mut queued = 0_usize;
    for inbox in &inboxes {
        match delivery::enqueue_activity(state.pool(), local_actor.id, inbox, activity).await {
            Ok(_) => queued += 1,
            Err(err) => warn!(?err, %inbox, "enqueue_activity failed"),
        }
    }
    queued
}

/// `reaction` テーブルの次の BIGSERIAL を消費して i64 を返す。
async fn next_reaction_id(state: &AppState) -> Result<i64, Response> {
    let row = sqlx::query!("SELECT nextval('reaction_id_seq') AS \"next!\"")
        .fetch_one(state.pool())
        .await;
    match row {
        Ok(r) => Ok(r.next),
        Err(err) => {
            error!(?err, "nextval(reaction_id_seq) failed");
            Err(StatusCode::SERVICE_UNAVAILABLE.into_response())
        }
    }
}

/// content が `:foo:` 形式ならローカル emoji 行を引く。Unicode の場合は
/// `Ok(None)` を返す。`:foo@host:` (remote 参照) はローカル emoji を作る
/// 経路が無いので 400 で弾く ── ローカル user が手動で remote 絵文字を
/// 指定する場面は M9 以降の remote emoji 自動学習で対応する。
async fn resolve_local_emoji(
    state: &AppState,
    content: &str,
) -> Result<Option<EmojiRow>, Response> {
    let Some(shortcode_raw) = content.strip_prefix(':').and_then(|s| s.strip_suffix(':')) else {
        // Unicode emoji。
        return Ok(None);
    };
    if shortcode_raw.contains('@') {
        return Err(bad_request(
            "remote emoji reactions are not supported yet; use a local shortcode or Unicode",
        ));
    }
    match repo::emoji::get_local_by_shortcode(state.pool(), shortcode_raw).await {
        Ok(Some(row)) => Ok(Some(row)),
        Ok(None) => Err(error_with_body(
            StatusCode::NOT_FOUND,
            "local emoji not found; run `sakurasato emoji import` or use Unicode",
        )),
        Err(err) => {
            error!(?err, shortcode = shortcode_raw, "emoji lookup failed");
            Err(StatusCode::SERVICE_UNAVAILABLE.into_response())
        }
    }
}

fn build_reaction_activity(
    state: &AppState,
    local_actor: &ActorRow,
    note_ap_id: &str,
    content: &str,
    emoji: Option<&EmojiRow>,
    ap_id: &str,
) -> JsonValue {
    let published = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    let (activity_type, tag) = if let Some(emoji) = emoji {
        // Misskey 互換: EmojiReact + tag に Emoji オブジェクト。
        let url = build_media_url(&state.config().server.host, &emoji.image_key);
        let emoji_ap_id = format!(
            "https://{host}/emojis/{shortcode}",
            host = state.config().server.host,
            shortcode = emoji.shortcode,
        );
        let tag = json!([{
            "type": "Emoji",
            "id": emoji_ap_id,
            "name": format!(":{}:", emoji.shortcode),
            "updated": emoji.updated_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "icon": {
                "type": "Image",
                "mediaType": emoji.media_type,
                "url": url,
            },
        }]);
        ("EmojiReact", Some(tag))
    } else {
        // Unicode (空文字 / 1 文字以上の絵文字) は Like で送る。Mastodon 互換性。
        ("Like", None)
    };

    let mut activity = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": ap_id,
        "type": activity_type,
        "actor": local_actor.ap_id,
        "object": note_ap_id,
        "published": published,
    });
    if !content.is_empty() {
        activity["content"] = JsonValue::String(content.into());
    }
    if let Some(tag) = tag {
        activity["tag"] = tag;
    }
    activity
}

fn validate_content(content: &str) -> Result<(), &'static str> {
    if content.is_empty() {
        return Err("content must not be empty");
    }
    if content.chars().count() > CONTENT_MAX {
        return Err("content exceeds the 256-character limit");
    }
    Ok(())
}

async fn resolve_local_actor(state: &AppState) -> Result<ActorRow, Response> {
    let host = &state.config().server.host;
    let user = &state.config().server.user;
    let row = repo::actor::get_by_username_host(state.pool(), user, host)
        .await
        .map_err(|err| {
            error!(?err, "reactions: local actor lookup failed");
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

fn bad_request(reason: &'static str) -> Response {
    error_with_body(StatusCode::BAD_REQUEST, reason)
}

fn error_with_body(status: StatusCode, reason: &str) -> Response {
    (status, Json(json!({"error": reason}))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_content_caps_length() {
        assert!(validate_content("").is_err());
        assert!(validate_content("👍").is_ok());
        assert!(validate_content(":blob:").is_ok());
        let big = "x".repeat(CONTENT_MAX + 1);
        assert!(validate_content(&big).is_err());
    }
}
