//! `POST /api/v1/reactions` / `DELETE /api/v1/reactions/{id}` (M8 PR2)。
//!
//! ローカル user が自分の Note にリアクションを付けて連合先に通知する経路。
//!
//! ## 流れ (`POST`)
//!
//! 1. body の `note_id` を解決し、`note` 行が見つかることを確認 ── local / remote
//!    どちらでも可。remote note への反応は `enqueue_reaction_delivery` 側で
//!    note 作者 inbox を必ず宛先に含めるので相手に届く。
//! 2. `content` を検証:
//!    - 空 / 長すぎは 400。
//!    - `:foo:` 形式ならローカル emoji 行を引いて `emoji_id` を紐付け、
//!      AP `tag: [Emoji]` を組み立てる。
//!    - Unicode は `emoji_id` = NULL、`tag` 無し。
//! 3. reaction 行を idempotent に insert ([`repo::reaction::insert_or_get`])。
//! 4. `EmojiReact` (custom emoji, `_misskey_reaction` 併載で旧 Misskey 互換)
//!    / `Like` (Unicode) Activity を組み立て、followers + note 作者の inbox
//!    (note が remote のときのみ) に `delivery_queue` 経由で push。
//!
//! ## 流れ (`DELETE`)
//!
//! 1. `reaction_id` で行を引く ── ローカル actor 所有でなければ 403。
//! 2. 元の `Like` / `EmojiReact` Activity を再構築して `Undo.object` に
//!    inline 埋め込み (URI 参照は Misskey 系で取りこぼし報告があるため)。
//! 3. followers + note 作者 (remote のみ) の inbox に配送。
//! 4. ローカル DB からも `reaction` 行を `delete_by_ap_id` で即時削除。

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use sakurasato_core::model::{ActorRow, EmojiRow};
use sakurasato_core::repo;
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use std::collections::BTreeSet;
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
    match create_reaction_core(&state, req.note_id, &req.content).await {
        Ok(outcome) => {
            let location = format!("/api/v1/reactions/{}", outcome.reaction.id);
            let body = ReactionResponse {
                id: outcome.reaction.id,
                ap_id: outcome.reaction.ap_id.clone(),
                note_id: outcome.reaction.note_id,
                content: outcome.reaction.content,
                emoji_id: outcome.reaction.emoji_id,
                queued_deliveries: outcome.queued_deliveries,
            };
            let mut response = (StatusCode::CREATED, Json(body)).into_response();
            if let Ok(hv) = HeaderValue::from_str(&location) {
                response
                    .headers_mut()
                    .insert(HeaderName::from_static("location"), hv);
            }
            response
        }
        Err(err) => reaction_core_error_to_local_response(&err),
    }
}

/// `reactions::create` の core 部分 (M14 #160 で抽出)。
///
/// 既存 `local_api/reactions::create` と新規 `miauth/reactions::create` が共有する
/// ロジックを 1 関数に集約。validate → local actor 解決 → note 取得 → emoji 解決 →
/// 決定論 `ap_id` 採番 → `insert_or_get` (冪等) → 新規なら `EmojiReact`/`Like` Activity
/// を配送 enqueue。
///
/// 返り値:
/// - `Ok(ReactionCoreOutcome)`: reaction 行と enqueue 件数。`queued_deliveries`
///   は 0 (= idempotent 再叩き) もあり得る
/// - `Err(ReactionCoreError)`: 入力 / I/O エラー (HTTP マップは呼び出し側責務)
pub(crate) async fn create_reaction_core(
    state: &AppState,
    note_id: i64,
    content: &str,
) -> Result<ReactionCoreOutcome, ReactionCoreError> {
    validate_content(content).map_err(|m| ReactionCoreError::BadRequest(m.to_string()))?;
    let local_actor = resolve_local_actor_or_err(state).await?;
    let note = match repo::note::get_by_id(state.pool(), note_id).await {
        Ok(Some(n)) => n,
        Ok(None) => return Err(ReactionCoreError::NoteNotFound),
        Err(err) => {
            error!(?err, note_id, "create_reaction_core: note lookup failed");
            return Err(ReactionCoreError::Internal);
        }
    };

    let emoji = resolve_local_emoji_or_err(state, content).await?;

    let Ok(reaction_id) = next_reaction_id_or_err(state).await else {
        return Err(ReactionCoreError::Internal);
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
        content,
        emoji.as_ref().map(|e| e.id),
    )
    .await
    {
        Ok(r) => r,
        Err(err) => {
            error!(?err, "create_reaction_core: insert_or_get failed");
            return Err(ReactionCoreError::Internal);
        }
    };

    let queued = if inserted.ap_id == ap_id {
        let activity = build_reaction_activity(
            state,
            &local_actor,
            &note.ap_id,
            content,
            emoji.as_ref(),
            &inserted.ap_id,
            inserted.created_at,
        );
        enqueue_reaction_delivery(state, &local_actor, note.actor_id, &activity).await
    } else {
        0
    };

    Ok(ReactionCoreOutcome {
        reaction: inserted,
        queued_deliveries: queued,
    })
}

/// `create_reaction_core` の成功結果。
#[derive(Debug, Clone)]
pub(crate) struct ReactionCoreOutcome {
    pub reaction: sakurasato_core::model::ReactionRow,
    pub queued_deliveries: usize,
}

/// `create_reaction_core` / `delete_reaction_core` の終端エラー。HTTP / Misskey
/// wire の status code は呼び出し側で別途マップする (= `local_api` と miauth で
/// 別の `error.code` 文字列を返したい)。
#[derive(Debug)]
pub(crate) enum ReactionCoreError {
    BadRequest(String),
    /// note 行が DB に居ない (= 404)。
    NoteNotFound,
    /// reaction 行が DB に居ない (= 404)。
    ReactionNotFound,
    /// `:shortcode:` 指定のローカル emoji が DB に無い (= 404)。
    EmojiNotFound,
    /// 削除しようとした reaction が local actor の所有ではない (= 403)。
    NotOwned,
    /// local actor 未 init / DB 不整合 (= 503)。
    LocalActorMissing,
    /// DB / シリアライズエラー (= 500)。
    Internal,
}

/// `bad_request` Helper を error 文字列向けに公開。
fn reaction_core_error_to_local_response(err: &ReactionCoreError) -> Response {
    match err {
        ReactionCoreError::BadRequest(msg) => error_with_body(StatusCode::BAD_REQUEST, msg),
        ReactionCoreError::NoteNotFound => error_with_body(StatusCode::NOT_FOUND, "note not found"),
        ReactionCoreError::ReactionNotFound => {
            error_with_body(StatusCode::NOT_FOUND, "reaction not found")
        }
        ReactionCoreError::EmojiNotFound => error_with_body(
            StatusCode::NOT_FOUND,
            "local emoji not found; run `sakurasato emoji import` or use Unicode",
        ),
        ReactionCoreError::NotOwned => {
            error_with_body(StatusCode::FORBIDDEN, "reaction not owned by local actor")
        }
        ReactionCoreError::LocalActorMissing => error_with_body(
            StatusCode::SERVICE_UNAVAILABLE,
            "local actor not initialized; run `sakurasato init`",
        ),
        ReactionCoreError::Internal => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

async fn resolve_local_actor_or_err(state: &AppState) -> Result<ActorRow, ReactionCoreError> {
    let host = &state.config().server.host;
    let user = &state.config().server.user;
    let row = repo::actor::get_by_username_host(state.pool(), user, host)
        .await
        .map_err(|err| {
            error!(?err, "reactions core: local actor lookup failed");
            ReactionCoreError::Internal
        })?;
    match row {
        Some(a) if a.is_local => Ok(a),
        _ => Err(ReactionCoreError::LocalActorMissing),
    }
}

async fn resolve_local_emoji_or_err(
    state: &AppState,
    content: &str,
) -> Result<Option<EmojiRow>, ReactionCoreError> {
    let Some(shortcode_raw) = content.strip_prefix(':').and_then(|s| s.strip_suffix(':')) else {
        return Ok(None);
    };
    if shortcode_raw.contains('@') {
        return Err(ReactionCoreError::BadRequest(
            "remote emoji reactions are not supported yet; use a local shortcode or Unicode".into(),
        ));
    }
    match repo::emoji::get_local_by_shortcode(state.pool(), shortcode_raw).await {
        Ok(Some(row)) => Ok(Some(row)),
        Ok(None) => Err(ReactionCoreError::EmojiNotFound),
        Err(err) => {
            error!(?err, shortcode = shortcode_raw, "emoji lookup failed");
            Err(ReactionCoreError::Internal)
        }
    }
}

async fn next_reaction_id_or_err(state: &AppState) -> Result<i64, ()> {
    sqlx::query!("SELECT nextval('reaction_id_seq') AS \"next!\"")
        .fetch_one(state.pool())
        .await
        .map(|r| r.next)
        .map_err(|err| {
            error!(?err, "nextval(reaction_id_seq) failed");
        })
}

pub async fn delete(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    match delete_reaction_core(&state, id).await {
        Ok(outcome) => (
            StatusCode::OK,
            Json(json!({
                "deleted": outcome.reaction_id,
                "queued_deliveries": outcome.queued_deliveries,
            })),
        )
            .into_response(),
        Err(err) => reaction_core_error_to_local_response(&err),
    }
}

/// `delete_reaction_core`: 指定 `reaction_id` を所有者検証してから Undo Reaction
/// を組み立てて配送 + DB 行削除。
///
/// 所有者一致しない場合 [`ReactionCoreError::NotOwned`]、行が無いとき
/// [`ReactionCoreError::ReactionNotFound`]。
pub(crate) async fn delete_reaction_core(
    state: &AppState,
    reaction_id: i64,
) -> Result<DeleteReactionOutcome, ReactionCoreError> {
    let local_actor = resolve_local_actor_or_err(state).await?;
    let Some(row) = fetch_reaction_by_id_or_err(state, reaction_id).await? else {
        return Err(ReactionCoreError::ReactionNotFound);
    };
    if row.actor_id != local_actor.id {
        return Err(ReactionCoreError::NotOwned);
    }
    let queued = build_and_dispatch_delete_core(state, &local_actor, &row).await;
    Ok(DeleteReactionOutcome {
        reaction_id: row.id,
        queued_deliveries: queued,
    })
}

/// **M14 #160**: `notes/reactions/delete` (Misskey 仕様) の `(noteId)` から
/// reaction を解決して `delete_reaction_core` に流す経路。
///
/// Misskey wire は note 単位で「自分の reaction」を消す。`(note_id, actor_id)` で
/// 1 行特定 (= UNIQUE 制約上 高々 1 件、複数 emoji を同じ note に付けるケースは
/// 「最初の 1 件」を消す挙動 ── Misskey 公式も同じ semantics)。
pub(crate) async fn delete_my_reaction_on_note_core(
    state: &AppState,
    note_id: i64,
) -> Result<DeleteReactionOutcome, ReactionCoreError> {
    let local_actor = resolve_local_actor_or_err(state).await?;
    let row = sqlx::query_as!(
        sakurasato_core::model::ReactionRow,
        r#"
        SELECT id, ap_id, note_id, actor_id, content, emoji_id, created_at
        FROM reaction
        WHERE note_id = $1 AND actor_id = $2
        ORDER BY created_at ASC
        LIMIT 1
        "#,
        note_id,
        local_actor.id,
    )
    .fetch_optional(state.pool())
    .await
    .map_err(|err| {
        error!(
            ?err,
            note_id, "delete_my_reaction_on_note_core: lookup failed"
        );
        ReactionCoreError::Internal
    })?;
    let Some(row) = row else {
        return Err(ReactionCoreError::ReactionNotFound);
    };
    let queued = build_and_dispatch_delete_core(state, &local_actor, &row).await;
    Ok(DeleteReactionOutcome {
        reaction_id: row.id,
        queued_deliveries: queued,
    })
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DeleteReactionOutcome {
    pub reaction_id: i64,
    pub queued_deliveries: usize,
}

async fn fetch_reaction_by_id_or_err(
    state: &AppState,
    id: i64,
) -> Result<Option<sakurasato_core::model::ReactionRow>, ReactionCoreError> {
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
            Err(ReactionCoreError::Internal)
        }
    }
}

/// 旧 `build_and_dispatch_delete` の戻り値を `Response` ではなく `queued` だけに
/// 縮約した core 版。両 wire (`local_api` / miauth) で再利用。
async fn build_and_dispatch_delete_core(
    state: &AppState,
    local_actor: &ActorRow,
    row: &sakurasato_core::model::ReactionRow,
) -> usize {
    let note_opt = match repo::note::get_by_id(state.pool(), row.note_id).await {
        Ok(n) => n,
        Err(err) => {
            error!(
                ?err,
                reaction_id = row.id,
                "DELETE core: note lookup failed"
            );
            return 0;
        }
    };
    let queued = if let Some(note) = note_opt {
        let emoji = match row.emoji_id {
            Some(eid) => match repo::emoji::get_by_id(state.pool(), eid).await {
                Ok(opt) => opt,
                Err(err) => {
                    warn!(
                        ?err,
                        reaction_id = row.id,
                        "DELETE core: emoji lookup failed"
                    );
                    None
                }
            },
            None => None,
        };
        let original = build_reaction_activity(
            state,
            local_actor,
            &note.ap_id,
            &row.content,
            emoji.as_ref(),
            &row.ap_id,
            row.created_at,
        );
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
            "object": original,
        });
        enqueue_reaction_delivery(state, local_actor, note.actor_id, &activity).await
    } else {
        // note が消えているケース ── URI 参照だけの Undo を followers にだけ送る。
        warn!(reaction_id = row.id, note_id = row.note_id, "note vanished");
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
        enqueue_reaction_delivery(state, local_actor, local_actor.id, &activity).await
    };
    if let Err(err) = repo::reaction::delete_by_ap_id(state.pool(), &row.ap_id).await {
        warn!(?err, reaction_id = row.id, "DELETE core: row delete failed");
    }
    queued
}

/// reaction Activity の配送先を組み立てて enqueue する。
///
/// 宛先 = (a) 自分のフォロワー全員 + (b) **note 作者の inbox** (note 作者が
/// remote の場合のみ)。(b) を入れないと「自分のフォロワーに含まれない他人の
/// remote note にリアクション」が相手に届かない (Misskey で言う「他人の投稿に
/// 絵文字リアクション → 相手に通知」が成立しない)。Nekonoverse の振り分けに
/// 揃えた挙動。
///
/// `shared_inbox_url` 優先 (大量フォロワーで配送圧縮) + `BTreeSet` で重複除去
/// (= 自分のフォロワーに note 作者が含まれていれば同 inbox は 1 行になる)。
///
/// `note_actor_id` が local actor の id と等しいなら (b) は no-op (ローカル
/// note → 作者は自分 → inbox 解決しない)。
async fn enqueue_reaction_delivery(
    state: &AppState,
    local_actor: &ActorRow,
    note_actor_id: i64,
    activity: &JsonValue,
) -> usize {
    let mut inboxes: BTreeSet<String> = BTreeSet::new();

    match repo::follow::list_accepted_inboxes(state.pool(), local_actor.id).await {
        Ok(list) => inboxes.extend(list),
        Err(err) => warn!(
            ?err,
            "enqueue_reaction_delivery: list_accepted_inboxes failed"
        ),
    }

    if note_actor_id != local_actor.id {
        match repo::actor::get_by_id(state.pool(), note_actor_id).await {
            Ok(Some(note_actor)) if !note_actor.is_local => {
                let inbox = note_actor.shared_inbox_url.unwrap_or(note_actor.inbox_url);
                inboxes.insert(inbox);
            }
            Ok(_) => {} // ローカル actor、もしくは行が消えている → 追加しない。
            Err(err) => warn!(
                ?err,
                note_actor_id, "enqueue_reaction_delivery: note actor lookup failed"
            ),
        }
    }

    let mut queued = 0_usize;
    for inbox in &inboxes {
        match delivery::enqueue_activity(state.pool(), local_actor.id, inbox, activity).await {
            Ok(_) => queued += 1,
            Err(err) => warn!(?err, %inbox, "enqueue_activity failed"),
        }
    }
    queued
}

fn build_reaction_activity(
    state: &AppState,
    local_actor: &ActorRow,
    note_ap_id: &str,
    content: &str,
    emoji: Option<&EmojiRow>,
    ap_id: &str,
    published: DateTime<Utc>,
) -> JsonValue {
    let published = published.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

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
        // 旧 Misskey (= EmojiReact 解釈系ではなく Like + `_misskey_reaction` だけ
        // 読む系統) と互換するため content と同じ値を併載する。新 Misskey は
        // EmojiReact を優先するので二重に表示されることはない。
        activity["_misskey_reaction"] = JsonValue::String(content.into());
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
