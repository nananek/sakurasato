//! `POST /api/v1/notes/{id}/renote` / `DELETE /api/v1/notes/{id}/renote` (#151)。
//!
//! ローカル user が自分以外の Note を boost / renote して連合先に通知する経路。
//! 受信側は M11 で `announce` テーブル + `dispatch::announce` 経由に完成済み。
//! 本ファイルは送出側で、同じ `announce` テーブルへ「自分の boost 行」も
//! insert する (= 送受信両方で同じ正規化、`UNIQUE (note_id, actor_id)` で
//! 1 user 1 target の Mastodon 互換 1:1 制約を担保)。
//!
//! ## 流れ (`POST`)
//!
//! 1. path の `note_id` を解決し、`note` 行が見つかることを確認 ── local /
//!    remote どちらでも可。remote note の renote は [`enqueue_announce_delivery`]
//!    で note 作者 inbox を必ず宛先に含めるので相手にも届く。
//! 2. visibility ガード: `public` / `unlisted` のみ renote 可能。`followers` /
//!    `direct` は 400 (Mastodon / Misskey 互換)。
//! 3. `announce_id_seq` で先に id を確保し、決定論的 `announce-<id>` の AP id
//!    を組み立てる ── Undo の突き合わせとログ追跡が容易になる。
//! 4. [`repo::announce::insert_or_get`] で idempotent に row を作る。`UNIQUE
//!    (note_id, actor_id)` が壁になるので、既存 row が返れば連合通知は再送しない。
//! 5. `Announce` Activity を組み立てて followers + note 作者 (remote のみ) の
//!    inbox に enqueue。
//!
//! ## 流れ (`DELETE`)
//!
//! 1. `(note_id, local_actor.id)` で `announce` row を引く ── 自分が renote
//!    していない note への DELETE は 404。
//! 2. 元の `Announce` Activity を再構築して `Undo.object` に inline 埋め込み
//!    (= reactions.rs と同じ理由、URI 参照だと Misskey 系で取りこぼし報告がある)。
//! 3. followers + note 作者 (remote のみ) の inbox に配送 enqueue。
//! 4. ローカル DB から `announce` row を `delete_by_ap_id` で即時削除。

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use sakurasato_core::model::ActorRow;
use sakurasato_core::repo;
use serde::Serialize;
use serde_json::{Value as JsonValue, json};
use std::collections::BTreeSet;
use tracing::{error, warn};

use crate::delivery;
use crate::state::AppState;

/// `Public` audience の URI (Mastodon / Misskey も同値で識別)。
const PUBLIC_AUDIENCE: &str = "https://www.w3.org/ns/activitystreams#Public";

#[derive(Debug, Serialize)]
pub struct AnnounceResponse {
    pub id: i64,
    pub ap_id: String,
    pub note_id: i64,
    pub queued_deliveries: usize,
}

pub async fn create(State(state): State<AppState>, Path(note_id): Path<i64>) -> Response {
    let local_actor = match resolve_local_actor(&state).await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let note = match repo::note::get_by_id(state.pool(), note_id).await {
        Ok(Some(n)) => n,
        Ok(None) => return error_with_body(StatusCode::NOT_FOUND, "note not found"),
        Err(err) => {
            error!(?err, "POST /api/v1/notes/{{id}}/renote: note lookup failed");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };

    // visibility ガード ── public / unlisted 以外は renote 不可。Mastodon /
    // Misskey と同じ振る舞いで、followers / direct は受信側で `Announce` を
    // 拒否されるので送ること自体に意味が無い。
    if !matches!(note.visibility.as_str(), "public" | "unlisted") {
        return error_with_body(
            StatusCode::BAD_REQUEST,
            "only public or unlisted notes can be renoted",
        );
    }

    // PR #155 review Finding 1: 自己 renote ガード ── Mastodon は自己 boost を
    // 422 で拒否する。許可するとフォロワーに無意味な Announce が配送され、
    // 受信側で `MAX_ATTEMPTS` 回リトライ後に dead に終わる (= ローカル
    // `announce` 行と remote の表示が乖離)。Misskey も同様。
    if note.actor_id == local_actor.id {
        return error_with_body(
            StatusCode::UNPROCESSABLE_ENTITY,
            "cannot renote your own note",
        );
    }

    // `announce.ap_id` は決定論的に組み立てたい (= Undo の object 再構築や
    // ログ追跡が容易) ので、insert 前に sequence の nextval を引いて id を
    // 確保する。BIGSERIAL は cycle しないので衝突は起きない。
    let seq = match next_announce_id(&state).await {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let ap_id = format!(
        "https://{host}/users/{user}/activities/announce-{seq}",
        host = state.config().server.host,
        user = local_actor.preferred_username,
    );
    let published_at = Utc::now();

    let row = match repo::announce::insert_or_get(
        state.pool(),
        &ap_id,
        note.id,
        local_actor.id,
        published_at,
    )
    .await
    {
        Ok(r) => r,
        Err(err) => {
            error!(
                ?err,
                "POST /api/v1/notes/{{id}}/renote: insert_or_get failed"
            );
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };

    // 既存 row (= 同じ `(note_id, actor_id)` で前回 renote 済み) が返った
    // ケースは連合通知を再送しない。冪等性。PR #155 round-2 F4 / round-3 F2:
    // 既存返却時は RFC 9110 に従って `200 OK` (= 「resource already existed」)、
    // 新規 insert のときだけ `201 Created` を返す。Location ヘッダはどちらも
    // 同じパスで OK ── 元 Note の id は変わらない。
    let is_new = row.ap_id == ap_id;
    let queued = if is_new {
        let activity =
            build_announce_activity(&local_actor, &note.ap_id, &row.ap_id, row.published_at);
        enqueue_announce_delivery(&state, &local_actor, note.actor_id, &activity).await
    } else {
        0
    };

    let body = AnnounceResponse {
        id: row.id,
        ap_id: row.ap_id.clone(),
        note_id: row.note_id,
        queued_deliveries: queued,
    };
    let status = if is_new {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    let location = format!("/api/v1/notes/{}/renote", row.note_id);
    let mut response = (status, Json(body)).into_response();
    if let Ok(hv) = HeaderValue::from_str(&location) {
        response
            .headers_mut()
            .insert(HeaderName::from_static("location"), hv);
    }
    response
}

pub async fn delete(State(state): State<AppState>, Path(note_id): Path<i64>) -> Response {
    let local_actor = match resolve_local_actor(&state).await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let row = match repo::announce::get_by_pair(state.pool(), note_id, local_actor.id).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return error_with_body(StatusCode::NOT_FOUND, "you have not renoted this note");
        }
        Err(err) => {
            error!(?err, note_id, "DELETE renote: get_by_pair failed");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    build_and_dispatch_undo(&state, &local_actor, row).await
}

async fn build_and_dispatch_undo(
    state: &AppState,
    local_actor: &ActorRow,
    row: sakurasato_core::model::AnnounceRow,
) -> Response {
    // Undo の object には元 Activity を inline 埋め込みする (reactions.rs と
    // 同じ理由)。再構築には note ap_id が要る。
    let note = match repo::note::get_by_id(state.pool(), row.note_id).await {
        Ok(Some(n)) => n,
        Ok(None) => {
            // 通常起き得ない (FK で消えるはず) が、起きたら URI 参照に
            // フォールバック。
            warn!(announce_id = row.id, note_id = row.note_id, "note vanished");
            return finalize_undo_with_uri_object(state, local_actor, &row).await;
        }
        Err(err) => {
            error!(
                ?err,
                announce_id = row.id,
                "DELETE renote: note lookup failed"
            );
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };

    let original = build_announce_activity(local_actor, &note.ap_id, &row.ap_id, row.published_at);
    let undo_id = format!(
        "https://{host}/users/{user}/activities/undo-announce-{id}",
        host = state.config().server.host,
        user = local_actor.preferred_username,
        id = row.id,
    );
    // PR #155 round-2 F2: Undo wrapper 自体にも audience を載せる ── 一部
    // 受信実装 (古い Misskey 系 / Pleroma の一部) は外側 to/cc でルーティング
    // するため、inner Announce にだけ付けても取りこぼされる。
    let mut activity = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": undo_id,
        "type": "Undo",
        "actor": local_actor.ap_id,
        "to": [PUBLIC_AUDIENCE],
        "object": original,
    });
    if let Some(followers) = local_actor.followers_url.as_ref() {
        activity["cc"] = json!([followers]);
    }

    // PR #155 review Finding 2: DB 削除を **先** に実行し、失敗時は 503 を
    // 返して enqueue しない ── reactions.rs と異なる方針。順序を逆にすると、
    // delete_by_ap_id 失敗時に Undo がキューに残ったまま `announce` 行が
    // 残存し、TUI の `viewer_renoted` が永続的に true 表示になる乖離が
    // 起きる。「先に消して enqueue は best-effort で warn」が write-ahead
    // セマンティクスとして安全。enqueue 自体は内部で warn 集約するので、
    // 1 件でも `delivery_queue` insert が成功すれば配送 worker が再試行する。
    //
    // PR #155 round-2 F1: rows_affected = 0 (= 別 request が先に消した) は
    // 200 で静かに返し、Undo 再配送をスキップする ── 受信側は同 undo_id を
    // 冪等に無視するが、`delivery_queue` 行が二重に積まれて worker が無駄
    // 再試行するのを避ける。
    match repo::announce::delete_by_ap_id(state.pool(), &row.ap_id).await {
        Ok(0) => {
            return (
                StatusCode::OK,
                Json(json!({
                    "deleted": row.id,
                    "queued_deliveries": 0,
                })),
            )
                .into_response();
        }
        Ok(_) => {}
        Err(err) => {
            error!(
                ?err,
                announce_id = row.id,
                "DELETE renote: row delete failed"
            );
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    }
    let queued = enqueue_announce_delivery(state, local_actor, note.actor_id, &activity).await;

    (
        StatusCode::OK,
        Json(json!({
            "deleted": row.id,
            "queued_deliveries": queued,
        })),
    )
        .into_response()
}

/// note 行が消失して元 Activity を再構築できないときの退避経路。Undo.object
/// に URI だけ載せて出す ── 受信側で取りこぼしの可能性はあるが、ローカル
/// DB の整合性 (= announce 行を削除する) は確保したい。
async fn finalize_undo_with_uri_object(
    state: &AppState,
    local_actor: &ActorRow,
    row: &sakurasato_core::model::AnnounceRow,
) -> Response {
    let undo_id = format!(
        "https://{host}/users/{user}/activities/undo-announce-{id}",
        host = state.config().server.host,
        user = local_actor.preferred_username,
        id = row.id,
    );
    // 主経路と同じく外側 to/cc audience を載せる (PR #155 round-2 F2)。
    let mut activity = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": undo_id,
        "type": "Undo",
        "actor": local_actor.ap_id,
        "to": [PUBLIC_AUDIENCE],
        "object": row.ap_id,
    });
    if let Some(followers) = local_actor.followers_url.as_ref() {
        activity["cc"] = json!([followers]);
    }
    // PR #155 review Finding 2: 主経路と同じく DB 削除を先に。
    // PR #155 round-2 F1: rows_affected = 0 は 200 で静かに返す (= 主経路と
    // 同じ idempotent 動作)。
    match repo::announce::delete_by_ap_id(state.pool(), &row.ap_id).await {
        Ok(0) => {
            return (
                StatusCode::OK,
                Json(json!({
                    "deleted": row.id,
                    "queued_deliveries": 0,
                })),
            )
                .into_response();
        }
        Ok(_) => {}
        Err(err) => {
            error!(
                ?err,
                announce_id = row.id,
                "DELETE renote (fallback): row delete failed"
            );
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    }
    // note が消えていれば note 作者 inbox の解決もできない。followers だけに送る。
    let queued = enqueue_announce_delivery(state, local_actor, local_actor.id, &activity).await;
    (
        StatusCode::OK,
        Json(json!({
            "deleted": row.id,
            "queued_deliveries": queued,
        })),
    )
        .into_response()
}

/// Announce Activity の配送先を組み立てて enqueue する。
///
/// 宛先 = (a) 自分のフォロワー全員 + (b) **note 作者の inbox** (note 作者が
/// remote の場合のみ)。(b) を入れないと「他人の remote note を boost」が
/// 元著者に通知されない (= Mastodon の boost で元著者通知が出ない、と相手
/// 側で困る)。`reactions.rs::enqueue_reaction_delivery` と同形。
async fn enqueue_announce_delivery(
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
            "enqueue_announce_delivery: list_accepted_inboxes failed"
        ),
    }

    if note_actor_id != local_actor.id {
        match repo::actor::get_by_id(state.pool(), note_actor_id).await {
            Ok(Some(note_actor)) if !note_actor.is_local => {
                let inbox = note_actor.shared_inbox_url.unwrap_or(note_actor.inbox_url);
                inboxes.insert(inbox);
            }
            Ok(_) => {} // ローカル actor or 行消失 → 追加しない。
            Err(err) => warn!(
                ?err,
                note_actor_id, "enqueue_announce_delivery: note actor lookup failed"
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

/// `announce` テーブルの次の BIGSERIAL を消費して i64 を返す。
async fn next_announce_id(state: &AppState) -> Result<i64, Response> {
    let row = sqlx::query!("SELECT nextval('announce_id_seq') AS \"next!\"")
        .fetch_one(state.pool())
        .await;
    match row {
        Ok(r) => Ok(r.next),
        Err(err) => {
            error!(?err, "nextval(announce_id_seq) failed");
            Err(StatusCode::SERVICE_UNAVAILABLE.into_response())
        }
    }
}

fn build_announce_activity(
    local_actor: &ActorRow,
    note_ap_id: &str,
    ap_id: &str,
    published: DateTime<Utc>,
) -> JsonValue {
    let published = published.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    // Mastodon の boost に倣い `to = [#Public]` / `cc = [followers]` の audience
    // を載せる ── public timeline 上で boost として識別される。`followers_url`
    // が未設定 (= まれな M3 以前のローカル actor 等) のときは cc を省略する。
    let mut activity = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": ap_id,
        "type": "Announce",
        "actor": local_actor.ap_id,
        "object": note_ap_id,
        "published": published,
        "to": [PUBLIC_AUDIENCE],
    });
    if let Some(followers) = local_actor.followers_url.as_ref() {
        activity["cc"] = json!([followers]);
    }
    activity
}

async fn resolve_local_actor(state: &AppState) -> Result<ActorRow, Response> {
    let host = &state.config().server.host;
    let user = &state.config().server.user;
    let row = repo::actor::get_by_username_host(state.pool(), user, host)
        .await
        .map_err(|err| {
            error!(?err, "renotes: local actor lookup failed");
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

fn error_with_body(status: StatusCode, reason: &str) -> Response {
    (status, Json(json!({"error": reason}))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_actor() -> ActorRow {
        ActorRow {
            id: 1,
            ap_id: "https://h/users/u".into(),
            preferred_username: "u".into(),
            host: "h".into(),
            display_name: None,
            summary: None,
            icon_url: None,
            image_url: None,
            inbox_url: "https://h/users/u/inbox".into(),
            shared_inbox_url: Some("https://h/inbox".into()),
            outbox_url: None,
            followers_url: Some("https://h/users/u/followers".into()),
            following_url: None,
            public_key_id: "https://h/users/u#main-key".into(),
            public_key_pem: String::new(),
            private_key_pem: None,
            ed25519_public_key_id: None,
            ed25519_public_key_pem: None,
            ed25519_private_key_pem: None,
            also_known_as: sqlx::types::Json(Vec::new()),
            moved_to_ap_id: None,
            is_local: true,
            actor_type: "Person".into(),
            manually_approves_followers: false,
            fetched_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn announce_activity_has_required_fields() {
        let actor = dummy_actor();
        let act = build_announce_activity(
            &actor,
            "https://other.test/notes/123",
            "https://h/users/u/activities/announce-42",
            Utc::now(),
        );
        assert_eq!(act["type"], "Announce");
        assert_eq!(act["actor"], "https://h/users/u");
        assert_eq!(act["object"], "https://other.test/notes/123");
        assert_eq!(act["to"][0], PUBLIC_AUDIENCE);
        assert_eq!(act["cc"][0], "https://h/users/u/followers");
        assert!(act.get("published").and_then(|v| v.as_str()).is_some());
    }

    #[test]
    fn announce_activity_omits_cc_without_followers_url() {
        let mut actor = dummy_actor();
        actor.followers_url = None;
        let act = build_announce_activity(
            &actor,
            "https://other.test/notes/123",
            "https://h/users/u/activities/announce-42",
            Utc::now(),
        );
        assert!(act.get("cc").is_none());
    }
}
